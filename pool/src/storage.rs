use soroban_sdk::{
    contracttype, map, panic_with_error, unwrap::UnwrapOptimized, vec, Address, Env, IntoVal, Map,
    String, Symbol, TryFromVal, Val, Vec,
};

use crate::{auctions::AuctionData, constants::MAX_RESERVES, pool::Positions, PoolError};

/********** Ledger Thresholds **********/

const ONE_DAY_LEDGERS: u32 = 17280; // assumes 5s a ledger

const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30; // ~ 30 days
const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS; // ~ 31 days

const LEDGER_THRESHOLD_SHARED: u32 = ONE_DAY_LEDGERS * 45; // ~ 45 days
const LEDGER_BUMP_SHARED: u32 = LEDGER_THRESHOLD_SHARED + ONE_DAY_LEDGERS; // ~ 46 days

const LEDGER_THRESHOLD_USER: u32 = ONE_DAY_LEDGERS * 100; // ~ 100 days
const LEDGER_BUMP_USER: u32 = LEDGER_THRESHOLD_USER + 20 * ONE_DAY_LEDGERS; // ~ 120 days

/********** Storage Types **********/

/// The pool's config
#[derive(Clone)]
#[contracttype]
pub struct PoolConfig {
    pub oracle: Address,      // the contract address of the oracle
    pub min_collateral: i128, // the minimum amount of collateral required to open a liability position
    pub bstop_rate: u32, // the rate the backstop takes on accrued debt interest, expressed in 7 decimals
    pub status: u32,     // the status of the pool
    pub max_positions: u32, // the maximum number of effective positions a single user can hold, and the max assets an auction can contain
}

/// The pool's emission config
#[derive(Clone)]
#[contracttype]
pub struct PoolEmissionConfig {
    pub config: u128,
    pub last_time: u64,
}

/// The configuration information about a reserve asset
#[derive(Clone, Debug)]
#[contracttype]
pub struct ReserveConfig {
    pub index: u32,       // the index of the reserve in the list
    pub decimals: u32,    // the decimals used in both the bToken and underlying contract
    pub c_factor: u32,    // the collateral factor for the reserve scaled expressed in 7 decimals
    pub l_factor: u32,    // the liability factor for the reserve scaled expressed in 7 decimals
    pub util: u32,        // the target utilization rate scaled expressed in 7 decimals
    pub max_util: u32,    // the maximum allowed utilization rate scaled expressed in 7 decimals
    pub r_base: u32, // the R0 value (base rate) in the interest rate formula scaled expressed in 7 decimals
    pub r_one: u32,  // the R1 value in the interest rate formula scaled expressed in 7 decimals
    pub r_two: u32,  // the R2 value in the interest rate formula scaled expressed in 7 decimals
    pub r_three: u32, // the R3 value in the interest rate formula scaled expressed in 7 decimals
    pub reactivity: u32, // the reactivity constant for the reserve scaled expressed in 7 decimals
    pub supply_cap: i128, // the total amount of underlying tokens that can be supplied to the reserve
    pub enabled: bool,    // the enabled flag of the reserve
}

#[derive(Clone)]
#[contracttype]
pub struct QueuedReserveInit {
    pub new_config: ReserveConfig,
    pub unlock_time: u64,
}

/// The data for a reserve asset
#[derive(Clone, Debug)]
#[contracttype]
pub struct ReserveData {
    pub d_rate: i128,   // the conversion rate from dToken to underlying with 12 decimals
    pub b_rate: i128,   // the conversion rate from bToken to underlying with 12 decimals
    pub ir_mod: i128,   // the interest rate curve modifier with 7 decimals
    pub b_supply: i128, // the total supply of b tokens, in the underlying token's decimals
    pub d_supply: i128, // the total supply of d tokens, in the underlying token's decimals
    pub backstop_credit: i128, // the amount of underlying tokens currently owed to the backstop
    pub last_time: u64, // the last block the data was updated
}

/// The emission data for the reserve b or d token
#[derive(Clone)]
#[contracttype]
pub struct ReserveEmissionData {
    pub expiration: u64,
    pub eps: u64,
    pub index: i128,
    pub last_time: u64,
}

/// The user emission data for the reserve b or d token
#[derive(Clone)]
#[contracttype]
pub struct UserEmissionData {
    pub index: i128,
    pub accrued: i128,
}

/********** Storage Key Types **********/

const ADMIN_KEY: &str = "Admin";
const PROPOSED_ADMIN_KEY: &str = "PropAdmin";
const NAME_KEY: &str = "Name";
const BACKSTOP_KEY: &str = "Backstop";
const BLND_TOKEN_KEY: &str = "BLNDTkn";
const POOL_CONFIG_KEY: &str = "Config";
const RES_LIST_KEY: &str = "ResList";
const POOL_EMIS_KEY: &str = "PoolEmis";

#[derive(Clone)]
#[contracttype]
pub struct UserReserveKey {
    user: Address,
    reserve_id: u32,
}

#[derive(Clone)]
#[contracttype]
pub struct AuctionKey {
    user: Address,  // the Address whose assets are involved in the auction
    auct_type: u32, // the type of auction taking place
}

#[derive(Clone)]
#[contracttype]
pub enum PoolDataKey {
    // A map of underlying asset's contract address to reserve config
    ResConfig(Address),
    // A map of underlying asset's contract address to queued reserve init
    ResInit(Address),
    // A map of underlying asset's contract address to reserve data
    ResData(Address),
    // The reserve's emission data
    EmisData(u32),
    // Map of positions in the pool for a user
    Positions(Address),
    // The emission information for a reserve asset for a user
    UserEmis(UserReserveKey),
    // The auction's data
    Auction(AuctionKey),
}

/********** Storage **********/

/// Bump the instance rent for the contract
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Fetch an entry in persistent storage that has a default value if it doesn't exist
fn get_persistent_default<K: IntoVal<Env, Val>, V: TryFromVal<Env, Val>, F: FnOnce() -> V>(
    e: &Env,
    key: &K,
    default: F,
    bump_threshold: u32,
    bump_amount: u32,
) -> V {
    if let Some(result) = e.storage().persistent().get::<K, V>(key) {
        e.storage()
            .persistent()
            .extend_ttl(key, bump_threshold, bump_amount);
        result
    } else {
        default()
    }
}

/********** User **********/

/// Fetch the user's positions or return an empty Positions struct
///
/// ### Arguments
/// * `user` - The address of the user
pub fn get_user_positions(e: &Env, user: &Address) -> Positions {
    let key = PoolDataKey::Positions(user.clone());
    get_persistent_default(
        e,
        &key,
        || Positions::env_default(e),
        LEDGER_THRESHOLD_USER,
        LEDGER_BUMP_USER,
    )
}

/// Set the user's positions
///
/// ### Arguments
/// * `user` - The address of the user
/// * `positions` - The new positions for the user
pub fn set_user_positions(e: &Env, user: &Address, positions: &Positions) {
    let key = PoolDataKey::Positions(user.clone());
    e.storage()
        .persistent()
        .set::<PoolDataKey, Positions>(&key, positions);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

/********** Admin **********/

/// Fetch the current admin Address
///
/// ### Panics
/// If the admin does not exist
pub fn get_admin(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&Symbol::new(e, ADMIN_KEY))
        .unwrap_optimized()
}

/// Set a new admin
///
/// ### Arguments
/// * `new_admin` - The Address for the admin
pub fn set_admin(e: &Env, new_admin: &Address) {
    e.storage()
        .instance()
        .set::<Symbol, Address>(&Symbol::new(e, ADMIN_KEY), new_admin);
}

/// Fetch the current proposed admin Address
///
/// ### Panics
/// If the admin does not exist
pub fn get_proposed_admin(e: &Env) -> Option<Address> {
    e.storage()
        .temporary()
        .get(&Symbol::new(e, PROPOSED_ADMIN_KEY))
}

/// Set a new proposed admin
///
/// ### Arguments
/// * `proposed_admin` - The Address for the proposed admin
pub fn set_proposed_admin(e: &Env, proposed_admin: &Address) {
    e.storage()
        .temporary()
        .set::<Symbol, Address>(&Symbol::new(e, PROPOSED_ADMIN_KEY), proposed_admin);
    e.storage().temporary().extend_ttl(
        &Symbol::new(e, PROPOSED_ADMIN_KEY),
        10 * ONE_DAY_LEDGERS,
        10 * ONE_DAY_LEDGERS,
    );
}

/********** Metadata **********/

/// Set a pool name
///
/// ### Arguments
/// * `name` - The Name of the pool
pub fn set_name(e: &Env, name: &String) {
    e.storage()
        .instance()
        .set::<Symbol, String>(&Symbol::new(e, NAME_KEY), name);
}

/********** Backstop **********/

/// Fetch the backstop ID for the pool
///
/// ### Panics
/// If no backstop is set
pub fn get_backstop(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&Symbol::new(e, BACKSTOP_KEY))
        .unwrap_optimized()
}

/// Set a new backstop ID
///
/// ### Arguments
/// * `backstop` - The address of the backstop
pub fn set_backstop(e: &Env, backstop: &Address) {
    e.storage()
        .instance()
        .set::<Symbol, Address>(&Symbol::new(e, BACKSTOP_KEY), backstop);
}

/********** External Token Contracts **********/

/// Fetch the BLND token ID
pub fn get_blnd_token(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&Symbol::new(e, BLND_TOKEN_KEY))
        .unwrap_optimized()
}

/// Set a new BLND token ID
///
/// ### Arguments
/// * `blnd_token_id` - The ID of the BLND token
pub fn set_blnd_token(e: &Env, blnd_token_id: &Address) {
    e.storage()
        .instance()
        .set::<Symbol, Address>(&Symbol::new(e, BLND_TOKEN_KEY), blnd_token_id);
}

/********** Pool Config **********/

/// Fetch the pool configuration
///
/// ### Panics
/// If the pool's config is not set
pub fn get_pool_config(e: &Env) -> PoolConfig {
    e.storage()
        .instance()
        .get(&Symbol::new(e, POOL_CONFIG_KEY))
        .unwrap_optimized()
}

/// Set the pool configuration
///
/// ### Arguments
/// * `config` - The contract address of the oracle
pub fn set_pool_config(e: &Env, config: &PoolConfig) {
    e.storage()
        .instance()
        .set::<Symbol, PoolConfig>(&Symbol::new(e, POOL_CONFIG_KEY), config);
}

/********** Reserve Config (ResConfig) **********/

/// Fetch the reserve data for an asset
///
/// ### Arguments
/// * `asset` - The contract address of the asset
///
/// ### Panics
/// If the reserve does not exist
pub fn get_res_config(e: &Env, asset: &Address) -> ReserveConfig {
    let key = PoolDataKey::ResConfig(asset.clone());
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
    e.storage()
        .persistent()
        .get::<PoolDataKey, ReserveConfig>(&key)
        .unwrap_optimized()
}

/// Set the reserve configuration for an asset
///
/// ### Arguments
/// * `asset` - The contract address of the asset
/// * `config` - The reserve configuration for the asset
pub fn set_res_config(e: &Env, asset: &Address, config: &ReserveConfig) {
    let key = PoolDataKey::ResConfig(asset.clone());
    e.storage()
        .persistent()
        .set::<PoolDataKey, ReserveConfig>(&key, config);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
}

/// Checks if a reserve exists for an asset
///
/// ### Arguments
/// * `asset` - The contract address of the asset
pub fn has_res(e: &Env, asset: &Address) -> bool {
    let key = PoolDataKey::ResConfig(asset.clone());
    e.storage().persistent().has(&key)
}

/// Fetch a queued reserve set
///
/// ### Arguments
/// * `asset` - The contract address of the asset
///
/// ### Panics
/// If the reserve set has not been queued
pub fn get_queued_reserve_set(e: &Env, asset: &Address) -> QueuedReserveInit {
    let key = PoolDataKey::ResInit(asset.clone());
    e.storage()
        .temporary()
        .get::<PoolDataKey, QueuedReserveInit>(&key)
        .unwrap_optimized()
}

/// Check if a reserve is actively queued
///
/// ### Arguments
/// * `asset` - The contract address of the asset
pub fn has_queued_reserve_set(e: &Env, asset: &Address) -> bool {
    let key = PoolDataKey::ResInit(asset.clone());
    e.storage().temporary().has(&key)
}

/// Set a new queued reserve set
///
/// ### Arguments
/// * `asset` - The contract address of the asset
/// * `config` - The reserve configuration for the asset
pub fn set_queued_reserve_set(e: &Env, res_init: &QueuedReserveInit, asset: &Address) {
    let key = PoolDataKey::ResInit(asset.clone());
    e.storage()
        .temporary()
        .set::<PoolDataKey, QueuedReserveInit>(&key, res_init);
    e.storage()
        .temporary()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

/// Delete a queued reserve set
///
/// ### Arguments
/// * `asset` - The contract address of the asset
///
/// ### Panics
/// If the reserve set has not been queued
pub fn del_queued_reserve_set(e: &Env, asset: &Address) {
    let key = PoolDataKey::ResInit(asset.clone());
    e.storage().temporary().remove(&key);
}

/********** Reserve Data (ResData) **********/

/// Fetch the reserve data for an asset
///
/// ### Arguments
/// * `asset` - The contract address of the asset
///
/// ### Panics
/// If the reserve does not exist
pub fn get_res_data(e: &Env, asset: &Address) -> ReserveData {
    let key = PoolDataKey::ResData(asset.clone());
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
    e.storage()
        .persistent()
        .get::<PoolDataKey, ReserveData>(&key)
        .unwrap_optimized()
}

/// Set the reserve data for an asset
///
/// ### Arguments
/// * `asset` - The contract address of the asset
/// * `data` - The reserve data for the asset
pub fn set_res_data(e: &Env, asset: &Address, data: &ReserveData) {
    let key = PoolDataKey::ResData(asset.clone());
    e.storage()
        .persistent()
        .set::<PoolDataKey, ReserveData>(&key, data);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
}

/********** Reserve List (ResList) **********/

/// Fetch the list of reserves
pub fn get_res_list(e: &Env) -> Vec<Address> {
    get_persistent_default(
        e,
        &Symbol::new(e, RES_LIST_KEY),
        || vec![e],
        LEDGER_THRESHOLD_SHARED,
        LEDGER_BUMP_SHARED,
    )
}

/// Add a reserve to the back of the list and returns the index
///
/// ### Arguments
/// * `asset` - The contract address of the underlying asset
///
/// ### Panics
/// If the number of reserves in the list exceeds 50
///
// @dev: Once added it can't be removed
pub fn push_res_list(e: &Env, asset: &Address) -> u32 {
    let mut res_list = get_res_list(e);
    if res_list.len() >= MAX_RESERVES {
        panic_with_error!(e, PoolError::BadRequest)
    }
    res_list.push_back(asset.clone());
    let new_index = res_list.len() - 1;
    e.storage()
        .persistent()
        .set::<Symbol, Vec<Address>>(&Symbol::new(e, RES_LIST_KEY), &res_list);
    e.storage().persistent().extend_ttl(
        &Symbol::new(e, RES_LIST_KEY),
        LEDGER_THRESHOLD_SHARED,
        LEDGER_BUMP_SHARED,
    );
    new_index
}

/********** Reserve Emissions **********/

/// Fetch the emission data for the reserve b or d token
///
/// ### Arguments
/// * `res_token_index` - The d/bToken index for the reserve
pub fn get_res_emis_data(e: &Env, res_token_index: &u32) -> Option<ReserveEmissionData> {
    let key = PoolDataKey::EmisData(*res_token_index);
    get_persistent_default(
        e,
        &key,
        || None,
        LEDGER_THRESHOLD_SHARED,
        LEDGER_BUMP_SHARED,
    )
}

/// Set the emission data for the reserve b or d token
///
/// ### Arguments
/// * `res_token_index` - The d/bToken index for the reserve
/// * `res_emis_data` - The new emission data for the reserve token
pub fn set_res_emis_data(e: &Env, res_token_index: &u32, res_emis_data: &ReserveEmissionData) {
    let key = PoolDataKey::EmisData(*res_token_index);
    e.storage()
        .persistent()
        .set::<PoolDataKey, ReserveEmissionData>(&key, res_emis_data);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
}

/********** User Emissions **********/

/// Fetch the users emission data for a reserve's b or d token
///
/// ### Arguments
/// * `user` - The address of the user
/// * `res_token_index` - The d/bToken index for the reserve
pub fn get_user_emissions(
    e: &Env,
    user: &Address,
    res_token_index: &u32,
) -> Option<UserEmissionData> {
    let key = PoolDataKey::UserEmis(UserReserveKey {
        user: user.clone(),
        reserve_id: *res_token_index,
    });
    get_persistent_default(e, &key, || None, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER)
}

/// Set the users emission data for a reserve's d or d token
///
/// ### Arguments
/// * `user` - The address of the user
/// * `res_token_index` - The d/bToken index for the reserve
/// * `data` - The new user emission d ata for the d/bToken
pub fn set_user_emissions(e: &Env, user: &Address, res_token_index: &u32, data: &UserEmissionData) {
    let key = PoolDataKey::UserEmis(UserReserveKey {
        user: user.clone(),
        reserve_id: *res_token_index,
    });
    e.storage()
        .persistent()
        .set::<PoolDataKey, UserEmissionData>(&key, data);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

/********** Pool Emissions **********/

/// Fetch the pool reserve emissions
pub fn get_pool_emissions(e: &Env) -> Map<u32, u64> {
    get_persistent_default(
        e,
        &Symbol::new(e, POOL_EMIS_KEY),
        || map![e],
        LEDGER_THRESHOLD_SHARED,
        LEDGER_BUMP_SHARED,
    )
}

/// Set the pool reserve emissions
///
/// ### Arguments
/// * `emissions` - The map of emissions by reserve token id to share of emissions as
///                 a percentage of 1e7 (e.g. 15% = 1500000)
pub fn set_pool_emissions(e: &Env, emissions: &Map<u32, u64>) {
    e.storage()
        .persistent()
        .set::<Symbol, Map<u32, u64>>(&Symbol::new(e, POOL_EMIS_KEY), emissions);
    e.storage().persistent().extend_ttl(
        &Symbol::new(e, POOL_EMIS_KEY),
        LEDGER_THRESHOLD_SHARED,
        LEDGER_BUMP_SHARED,
    );
}

/********** Auctions ***********/

/// Fetch the auction data for an auction
///
/// ### Arguments
/// * `auction_type` - The type of auction
/// * `user` - The user who is auctioning off assets
///
/// ### Panics
/// If the auction does not exist
pub fn get_auction(e: &Env, auction_type: &u32, user: &Address) -> AuctionData {
    let key = PoolDataKey::Auction(AuctionKey {
        user: user.clone(),
        auct_type: *auction_type,
    });
    e.storage()
        .temporary()
        .get::<PoolDataKey, AuctionData>(&key)
        .unwrap_optimized()
}

/// Check if an auction exists for the given type and user
///
/// ### Arguments
/// * `auction_type` - The type of auction
/// * `user` - The user who is auctioning off assets
pub fn has_auction(e: &Env, auction_type: &u32, user: &Address) -> bool {
    let key = PoolDataKey::Auction(AuctionKey {
        user: user.clone(),
        auct_type: *auction_type,
    });
    e.storage().temporary().has(&key)
}

/// Set the the starting block for an auction
///
/// ### Arguments
/// * `auction_type` - The type of auction
/// * `user` - The user who is auctioning off assets
/// * `auction_data` - The auction data
pub fn set_auction(e: &Env, auction_type: &u32, user: &Address, auction_data: &AuctionData) {
    let key = PoolDataKey::Auction(AuctionKey {
        user: user.clone(),
        auct_type: *auction_type,
    });
    e.storage()
        .temporary()
        .set::<PoolDataKey, AuctionData>(&key, auction_data);
    e.storage()
        .temporary()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
}

/// Remove an auction
///
/// ### Arguments
/// * `auction_type` - The type of auction
/// * `user` - The user who is auctioning off assets
pub fn del_auction(e: &Env, auction_type: &u32, user: &Address) {
    let key = PoolDataKey::Auction(AuctionKey {
        user: user.clone(),
        auct_type: *auction_type,
    });
    e.storage().temporary().remove(&key);
}
