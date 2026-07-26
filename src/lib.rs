#![no_std]
//! Reference Soroswap-compatible constant-product AMM for ConfiBatch.
//!
//! Only a batch's net imbalance is executed as one public trade. The contract
//! uses the same constant-product output formula and 0.3% fee as
//! Soroswap/UniswapV2. Its swap interface expects callers to transfer input
//! tokens before invocation; router-based integrations may pull them instead.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

// The first mint permanently locks this many unowned shares. This prevents the
// pool from returning to zero supply and mitigates first-depositor share-price
// inflation. The initial seed must exceed the lock.
const MINIMUM_LIQUIDITY: i128 = 1000;
const TTL_DAY: u32 = 17_280;
const TTL_BUMP: u32 = 30 * TTL_DAY;
const TTL_THRESH: u32 = 20 * TTL_DAY;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum AmmError {
    NotInitialized = 1,
    NotAdmin = 2,
    BadToken = 3,
    Slippage = 4,
    NoLiquidity = 5,
    Overflow = 6,
    NoInput = 7,
    InsufficientShares = 8,
}

#[contracttype]
enum DataKey {
    Admin,
    TokenA,
    TokenB,
    ReserveA,
    ReserveB,
    LpSupply,
    LpBalance(Address),
}

#[contract]
pub struct MiniAmm;

#[cfg(not(test))]
#[contractimpl]
impl MiniAmm {
    /// Fresh pools are configured atomically with deployment, closing the
    /// deploy-to-init window in which another account could claim the AMM.
    pub fn __constructor(env: Env, admin: Address, token_a: Address, token_b: Address) {
        initialize_amm(&env, &admin, &token_a, &token_b).unwrap();
    }
}

#[contractimpl]
impl MiniAmm {
    pub fn init(env: Env, admin: Address, token_a: Address, token_b: Address) -> Result<(), AmmError> {
        initialize_amm(&env, &admin, &token_a, &token_b)
    }

    /// Set reserves from the contract's actual token balances after the admin
    /// seeds liquidity.
    pub fn sync(env: Env) -> Result<(), AmmError> {
        require_admin(&env)?;
        let a: Address = env.storage().instance().get(&DataKey::TokenA).ok_or(AmmError::NotInitialized)?;
        let b: Address = env.storage().instance().get(&DataKey::TokenB).ok_or(AmmError::NotInitialized)?;
        let here = env.current_contract_address();
        let ra = token::TokenClient::new(&env, &a).balance(&here);
        let rb = token::TokenClient::new(&env, &b).balance(&here);
        env.storage().instance().set(&DataKey::ReserveA, &ra);
        env.storage().instance().set(&DataKey::ReserveB, &rb);
        bump_instance(&env);
        Ok(())
    }

    /// Permissionless keeper for quiet pools. Mutating paths also renew the
    /// instance entry, but an idle AMM must remain restorable before expiry.
    pub fn bump_ttl(env: Env) {
        bump_instance(&env);
    }

    pub fn reserves(env: Env) -> (i128, i128) {
        (
            env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0),
            env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0),
        )
    }

    /// Constant-product output with a 0.3% fee (UniswapV2/Soroswap formula).
    pub fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> Result<i128, AmmError> {
        let (rin, rout) = reserve_in_out(&env, &token_in)?;
        if rin <= 0 || rout <= 0 {
            return Err(AmmError::NoLiquidity);
        }
        amount_out_cp(rin, rout, amount_in)
    }

    /// Return the token-B amount that matches `amount_a` at the current reserve
    /// ratio. Supplying this counter-leg avoids donating an over-supplied side
    /// to existing LPs. This read-only quote returns `NoLiquidity` when the pool
    /// has not been seeded.
    pub fn quote_add(env: Env, amount_a: i128) -> Result<i128, AmmError> {
        if amount_a <= 0 {
            return Err(AmmError::NoInput);
        }
        let ra: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0);
        let rb: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0);
        if ra <= 0 || rb <= 0 {
            return Err(AmmError::NoLiquidity);
        }
        Ok(amount_a.checked_mul(rb).ok_or(AmmError::Overflow)? / ra)
    }

    /// Execute a swap. The caller PUSHes the input of `token_in` to this contract
    /// BEFORE calling; we compute the output against the pre-input reserves, update
    /// reserves, and send the output to `to`. Returns amount_out. (Soroswap maps:
    /// amount_in, amount_out_min, path=[token_in,token_out], to.)
    ///
    /// The caller-supplied `amount_in` is informational. The payable input is
    /// derived from the contract's token balance minus the stored reserve, so
    /// output can only be released against tokens actually received.
    pub fn swap(
        env: Env,
        token_in: Address,
        amount_in: i128,
        amount_out_min: i128,
        to: Address,
    ) -> Result<i128, AmmError> {
        let _ = amount_in;
        let a: Address = env.storage().instance().get(&DataKey::TokenA).ok_or(AmmError::NotInitialized)?;
        let b: Address = env.storage().instance().get(&DataKey::TokenB).ok_or(AmmError::NotInitialized)?;
        let token_out = if token_in == a { b.clone() } else if token_in == b { a.clone() } else { return Err(AmmError::BadToken); };

        let (rin, rout) = reserve_in_out(&env, &token_in)?;
        if rin <= 0 || rout <= 0 {
            return Err(AmmError::NoLiquidity);
        }
        // The balance delta is the authoritative input amount.
        let here = env.current_contract_address();
        let bal_in = token::TokenClient::new(&env, &token_in).balance(&here);
        let real_in = bal_in.checked_sub(rin).ok_or(AmmError::Overflow)?;
        if real_in <= 0 {
            return Err(AmmError::NoInput);
        }
        let amount_out = amount_out_cp(rin, rout, real_in)?;
        if amount_out < amount_out_min || amount_out <= 0 {
            return Err(AmmError::Slippage);
        }
        // The input has already arrived, so only reserve accounting and output move here.
        let new_in = rin.checked_add(real_in).ok_or(AmmError::Overflow)?;
        let new_out = rout.checked_sub(amount_out).ok_or(AmmError::Overflow)?;
        if token_in == a {
            env.storage().instance().set(&DataKey::ReserveA, &new_in);
            env.storage().instance().set(&DataKey::ReserveB, &new_out);
        } else {
            env.storage().instance().set(&DataKey::ReserveB, &new_in);
            env.storage().instance().set(&DataKey::ReserveA, &new_out);
        }
        bump_instance(&env);
        token::TokenClient::new(&env, &token_out).transfer(&here, &to, &amount_out);
        Ok(amount_out)
    }

    // Constant-product LP shares follow UniswapV2-style accounting. The first
    // provider sets the ratio; later providers mint shares pro rata.
    pub fn add_liquidity(env: Env, from: Address, amount_a: i128, amount_b: i128) -> Result<i128, AmmError> {
        // A zero floor preserves the original entrypoint's behavior.
        Self::add_liquidity_min(env, from, amount_a, amount_b, 0)
    }

    /// add_liquidity with a caller-supplied minimum-shares floor. Reverts Slippage (leaving
    /// reserves/supply/balances untouched) if the shares this add would mint fall below `min_shares`
    /// — the LP's protection against adding into a ratio that shifted since they quoted. Pair
    /// quote_add(amount_a) to derive amount_b, then pass a min_shares just under the expected mint.
    pub fn add_liquidity_min(env: Env, from: Address, amount_a: i128, amount_b: i128, min_shares: i128) -> Result<i128, AmmError> {
        from.require_auth();
        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::NoInput);
        }
        let a: Address = env.storage().instance().get(&DataKey::TokenA).ok_or(AmmError::NotInitialized)?;
        let b: Address = env.storage().instance().get(&DataKey::TokenB).ok_or(AmmError::NotInitialized)?;
        let ra: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0);
        let rb: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0);
        let supply: i128 = env.storage().instance().get(&DataKey::LpSupply).unwrap_or(0);
        // On the first mint, total supply includes the permanent liquidity lock
        // while the provider receives only the remainder.
        let (minted, supply_add) = if supply <= 0 {
            let liq = isqrt(amount_a.checked_mul(amount_b).ok_or(AmmError::Overflow)?);
            if liq <= MINIMUM_LIQUIDITY {
                return Err(AmmError::NoInput);
            }
            (liq - MINIMUM_LIQUIDITY, liq)
        } else {
            // The smaller contribution ratio limits the minted shares.
            let sa = amount_a.checked_mul(supply).ok_or(AmmError::Overflow)? / ra;
            let sb = amount_b.checked_mul(supply).ok_or(AmmError::Overflow)? / rb;
            let m = if sa < sb { sa } else { sb };
            (m, m)
        };
        // Check the floor before transferring tokens or writing state.
        if minted < min_shares {
            return Err(AmmError::Slippage);
        }
        if minted <= 0 {
            return Err(AmmError::NoInput);
        }
        let here = env.current_contract_address();
        token::TokenClient::new(&env, &a).transfer(&from, &here, &amount_a);
        token::TokenClient::new(&env, &b).transfer(&from, &here, &amount_b);
        env.storage().instance().set(&DataKey::ReserveA, &ra.checked_add(amount_a).ok_or(AmmError::Overflow)?);
        env.storage().instance().set(&DataKey::ReserveB, &rb.checked_add(amount_b).ok_or(AmmError::Overflow)?);
        env.storage().instance().set(&DataKey::LpSupply, &supply.checked_add(supply_add).ok_or(AmmError::Overflow)?);
        let bal: i128 = env.storage().instance().get(&DataKey::LpBalance(from.clone())).unwrap_or(0);
        env.storage().instance().set(&DataKey::LpBalance(from), &bal.checked_add(minted).ok_or(AmmError::Overflow)?);
        bump_instance(&env);
        Ok(minted)
    }

    /// Burn `shares` and return pro-rata reserves to `from`.
    pub fn remove_liquidity(env: Env, from: Address, shares: i128) -> Result<(i128, i128), AmmError> {
        // Zero floors preserve the original entrypoint's behavior.
        Self::remove_liquidity_min(env, from, shares, 0, 0)
    }

    /// remove_liquidity with caller-supplied minimum-out floors. Reverts Slippage (leaving the pool
    /// untouched) if the pro-rata payout on either side would fall below min_a / min_b — protects an
    /// exiting LP from a reserve shift (e.g. a swap sandwiched into the same ledger) since the last quote.
    pub fn remove_liquidity_min(env: Env, from: Address, shares: i128, min_a: i128, min_b: i128) -> Result<(i128, i128), AmmError> {
        from.require_auth();
        if shares <= 0 {
            return Err(AmmError::NoInput);
        }
        let bal: i128 = env.storage().instance().get(&DataKey::LpBalance(from.clone())).unwrap_or(0);
        if shares > bal {
            return Err(AmmError::InsufficientShares);
        }
        let a: Address = env.storage().instance().get(&DataKey::TokenA).ok_or(AmmError::NotInitialized)?;
        let b: Address = env.storage().instance().get(&DataKey::TokenB).ok_or(AmmError::NotInitialized)?;
        let ra: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0);
        let rb: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0);
        let supply: i128 = env.storage().instance().get(&DataKey::LpSupply).unwrap_or(0);
        if supply <= 0 {
            return Err(AmmError::NoLiquidity);
        }
        let out_a = shares.checked_mul(ra).ok_or(AmmError::Overflow)? / supply;
        let out_b = shares.checked_mul(rb).ok_or(AmmError::Overflow)? / supply;
        // Check both floors before writing state.
        if out_a < min_a || out_b < min_b {
            return Err(AmmError::Slippage);
        }
        env.storage().instance().set(&DataKey::ReserveA, &(ra - out_a));
        env.storage().instance().set(&DataKey::ReserveB, &(rb - out_b));
        env.storage().instance().set(&DataKey::LpSupply, &(supply - shares));
        env.storage().instance().set(&DataKey::LpBalance(from.clone()), &(bal - shares));
        bump_instance(&env);
        let here = env.current_contract_address();
        if out_a > 0 { token::TokenClient::new(&env, &a).transfer(&here, &from, &out_a); }
        if out_b > 0 { token::TokenClient::new(&env, &b).transfer(&here, &from, &out_b); }
        Ok((out_a, out_b))
    }

    pub fn lp_balance(env: Env, addr: Address) -> i128 {
        env.storage().instance().get(&DataKey::LpBalance(addr)).unwrap_or(0)
    }

    pub fn lp_supply(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::LpSupply).unwrap_or(0)
    }
}

fn initialize_amm(
    env: &Env,
    admin: &Address,
    token_a: &Address,
    token_b: &Address,
) -> Result<(), AmmError> {
    if env.storage().instance().has(&DataKey::Admin) {
        return Err(AmmError::NotAdmin);
    }
    if token_a == token_b
        || *token_a == env.current_contract_address()
        || *token_b == env.current_contract_address()
    {
        return Err(AmmError::BadToken);
    }
    env.storage().instance().set(&DataKey::Admin, admin);
    env.storage().instance().set(&DataKey::TokenA, token_a);
    env.storage().instance().set(&DataKey::TokenB, token_b);
    env.storage().instance().set(&DataKey::ReserveA, &0i128);
    env.storage().instance().set(&DataKey::ReserveB, &0i128);
    bump_instance(env);
    Ok(())
}

fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESH, TTL_BUMP);
}

// Integer square root using Newton's method.
fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    // Compute ceil(x / 2) without evaluating x + 1.
    let mut y = x / 2 + x % 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

// Constant-product output with a 0.3% fee and checked arithmetic.
fn amount_out_cp(rin: i128, rout: i128, amount_in: i128) -> Result<i128, AmmError> {
    if amount_in <= 0 {
        return Err(AmmError::NoInput);
    }
    let amount_in_fee = amount_in.checked_mul(997).ok_or(AmmError::Overflow)?;
    let num = rout.checked_mul(amount_in_fee).ok_or(AmmError::Overflow)?;
    let den = rin.checked_mul(1000).ok_or(AmmError::Overflow)?.checked_add(amount_in_fee).ok_or(AmmError::Overflow)?;
    if den <= 0 {
        return Err(AmmError::NoLiquidity);
    }
    Ok(num / den)
}

fn reserve_in_out(env: &Env, token_in: &Address) -> Result<(i128, i128), AmmError> {
    let a: Address = env.storage().instance().get(&DataKey::TokenA).ok_or(AmmError::NotInitialized)?;
    let b: Address = env.storage().instance().get(&DataKey::TokenB).ok_or(AmmError::NotInitialized)?;
    let ra: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap_or(0);
    let rb: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap_or(0);
    if *token_in == a {
        Ok((ra, rb))
    } else if *token_in == b {
        Ok((rb, ra))
    } else {
        Err(AmmError::BadToken)
    }
}

fn require_admin(env: &Env) -> Result<(), AmmError> {
    let admin: Address = env.storage().instance().get(&DataKey::Admin).ok_or(AmmError::NotInitialized)?;
    admin.require_auth();
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::{StellarAssetClient, TokenClient};

    #[test]
    fn isqrt_handles_i128_max_without_overflow() {
        let root = isqrt(i128::MAX);
        assert!(root > 0);
        assert!(root <= i128::MAX / root);
        assert!((root + 1) > i128::MAX / (root + 1));
    }

    // a fresh AMM seeded with 1,000,000 of each token (reserves synced to the real balances)
    fn seed(env: &Env) -> (MiniAmmClient<'static>, Address, Address, Address) {
        let admin = Address::generate(env);
        let ta = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let tb = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let amm_id = env.register(MiniAmm, ());
        let amm = MiniAmmClient::new(env, &amm_id);
        amm.init(&admin, &ta, &tb);
        StellarAssetClient::new(env, &ta).mint(&amm_id, &1_000_000);
        StellarAssetClient::new(env, &tb).mint(&amm_id, &1_000_000);
        amm.sync();
        (amm, ta, tb, amm_id)
    }

    // AMM-1: calling swap WITHOUT pushing the input pays out nothing and leaves liquidity intact.
    #[test]
    fn swap_without_pushing_input_is_rejected_no_drain() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb, amm_id) = seed(&env);
        let attacker = Address::generate(&env);
        let res = amm.try_swap(&ta, &500_000, &0, &attacker); // claims input, pushes nothing
        assert!(res.is_err(), "swap with no pushed input must be rejected");
        assert_eq!(TokenClient::new(&env, &tb).balance(&attacker), 0, "attacker received nothing");
        assert_eq!(TokenClient::new(&env, &tb).balance(&amm_id), 1_000_000, "AMM liquidity intact");
    }

    // the documented push-then-swap path still works and gives a sane constant-product output.
    #[test]
    fn honest_push_then_swap_pays_out() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb, amm_id) = seed(&env);
        let user = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&user, &100_000);
        TokenClient::new(&env, &ta).transfer(&user, &amm_id, &100_000); // PUSH first
        let out = amm.swap(&ta, &100_000, &0, &user);
        assert!(out > 0 && out < 100_000, "sane constant-product output");
        assert_eq!(TokenClient::new(&env, &tb).balance(&user), out);
    }

    // AMM-1: lying about amount_in cannot inflate the payout — it is bound to the REAL pushed value.
    #[test]
    fn lying_about_amount_in_cannot_inflate_payout() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, _tb, amm_id) = seed(&env);
        let user = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&user, &100_000);
        TokenClient::new(&env, &ta).transfer(&user, &amm_id, &100_000); // pushed 100k
        let out = amm.swap(&ta, &1_000_000, &0, &user); // claims 10x
        let expected = (1_000_000i128 * (100_000 * 997)) / (1_000_000 * 1000 + 100_000 * 997);
        assert_eq!(out, expected, "payout bound to the pushed 100k, not the claimed 1M");
    }

    // a fresh (un-seeded) AMM so add_liquidity is the first liquidity
    fn fresh(env: &Env) -> (MiniAmmClient<'static>, Address, Address) {
        let admin = Address::generate(env);
        let ta = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let tb = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let amm = MiniAmmClient::new(env, &env.register(MiniAmm, ()));
        amm.init(&admin, &ta, &tb);
        (amm, ta, tb)
    }

    // LP: add liquidity (first = sqrt(a*b), then pro-rata) and remove it for the pro-rata reserves back.
    #[test]
    fn lp_add_then_remove_roundtrip() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        let lp1 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp1, &1_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp1, &1_000_000);
        // first mint = sqrt(a*b) = 1_000_000, but MINIMUM_LIQUIDITY (1000) is permanently locked, so the
        // provider is credited 999_000 while LpSupply carries the full 1_000_000.
        assert_eq!(amm.add_liquidity(&lp1, &1_000_000, &1_000_000), 999_000, "first LP mints sqrt(a*b) - MIN_LIQ");
        assert_eq!(amm.lp_supply(), 1_000_000, "supply includes the locked MIN_LIQ");
        assert_eq!(amm.reserves(), (1_000_000, 1_000_000));

        let lp2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp2, &500_000);
        StellarAssetClient::new(&env, &tb).mint(&lp2, &500_000);
        assert_eq!(amm.add_liquidity(&lp2, &500_000, &500_000), 500_000, "second LP mints pro-rata (unaffected by lock)");
        assert_eq!(amm.reserves(), (1_500_000, 1_500_000));

        let (a_out, b_out) = amm.remove_liquidity(&lp1, &999_000);
        assert_eq!((a_out, b_out), (999_000, 999_000), "LP1 gets its pro-rata share back");
        assert_eq!(amm.lp_balance(&lp1), 0);
        assert_eq!(amm.reserves(), (501_000, 501_000));
        // LP1 recovers 999_000 of its 1_000_000 — the 1000 forfeited to the lock stays in the pool forever.
        assert_eq!(TokenClient::new(&env, &ta).balance(&lp1), 999_000);
        assert!(amm.lp_supply() >= 1_000, "the locked MIN_LIQ can never be removed, so supply never hits 0");
    }

    #[test]
    fn remove_more_than_held_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp, &1_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp, &1_000_000);
        amm.add_liquidity(&lp, &1_000_000, &1_000_000); // credits 999_000 (MIN_LIQ locked)
        assert!(amm.try_remove_liquidity(&lp, &2_000_000).is_err(), "cannot burn more shares than held");
    }

    #[test]
    fn first_mint_locks_minimum_liquidity() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp, &1_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp, &1_000_000);
        // a first seed whose sqrt(a*b) <= MINIMUM_LIQUIDITY (1000) reverts — it can't cover the lock.
        assert!(amm.try_add_liquidity(&lp, &1_000, &1_000).is_err(), "seed == MIN_LIQ is too small (sqrt=1000)");
        assert!(amm.try_add_liquidity(&lp, &900, &900).is_err(), "seed below MIN_LIQ reverts");
        // a first seed just above the lock mints (sqrt - MIN_LIQ) to the provider; the diff stays locked.
        assert_eq!(amm.add_liquidity(&lp, &4_000, &4_000), 3_000, "sqrt(16e6)=4000, minus 1000 locked");
        assert_eq!(amm.lp_supply(), 4_000, "supply carries provider shares + the locked 1000");
        assert_eq!(amm.lp_balance(&lp), 3_000, "provider is credited only the unlocked shares");
    }

    // quote_add returns the balanced counter-leg; feeding it back makes sa == sb so the pro-rata
    // mint donates nothing (min(sa,sb) is not clipped). Also rejects an un-seeded pool.
    #[test]
    fn quote_add_balances_the_ratio_no_donation() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        // un-seeded pool -> ratio undefined
        assert!(amm.try_quote_add(&1_000).is_err(), "quote on empty pool must fail (NoLiquidity)");
        // seed at a 2:1 ratio (reserveA:reserveB)
        let lp1 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp1, &2_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp1, &1_000_000);
        amm.add_liquidity(&lp1, &2_000_000, &1_000_000);
        assert_eq!(amm.reserves(), (2_000_000, 1_000_000));
        // ideal B for 400_000 A at a 2:1 pool = 200_000
        assert_eq!(amm.quote_add(&400_000), 200_000, "quote_add = amount_a * rb / ra");
        // adding the quoted counter-leg mints the full pro-rata shares (sa == sb, nothing clipped)
        let lp2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp2, &400_000);
        StellarAssetClient::new(&env, &tb).mint(&lp2, &200_000);
        let supply_before = amm.lp_supply();
        let minted = amm.add_liquidity(&lp2, &400_000, &200_000);
        // sa = 400_000*supply/2_000_000 = supply/5 ; sb = 200_000*supply/1_000_000 = supply/5 -> equal
        assert_eq!(minted, supply_before / 5, "balanced add mints the un-clipped pro-rata shares");
    }

    // add_liquidity_min reverts Slippage (pool untouched) when the mint would fall below the floor.
    #[test]
    fn add_liquidity_min_floors_the_mint() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        let lp1 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp1, &1_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp1, &1_000_000);
        amm.add_liquidity(&lp1, &1_000_000, &1_000_000); // supply now 1_000_000
        let lp2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp2, &100_000);
        StellarAssetClient::new(&env, &tb).mint(&lp2, &100_000);
        // this add mints 100_000 shares; demanding 100_001 must revert and touch nothing
        let supply_before = amm.lp_supply();
        assert!(amm.try_add_liquidity_min(&lp2, &100_000, &100_000, &100_001).is_err(), "floor above mint -> Slippage");
        assert_eq!(amm.lp_supply(), supply_before, "reverted add left supply untouched");
        assert_eq!(amm.lp_balance(&lp2), 0, "reverted add credited no shares");
        assert_eq!(TokenClient::new(&env, &ta).balance(&lp2), 100_000, "reverted add pulled no tokens");
        // demanding exactly the mint succeeds
        assert_eq!(amm.add_liquidity_min(&lp2, &100_000, &100_000, &100_000), 100_000, "floor == mint -> ok");
    }

    // remove_liquidity_min reverts Slippage (pool untouched) when either side would pay below the floor.
    #[test]
    fn remove_liquidity_min_floors_the_payout() {
        let env = Env::default();
        env.mock_all_auths();
        let (amm, ta, tb) = fresh(&env);
        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp, &1_000_000);
        StellarAssetClient::new(&env, &tb).mint(&lp, &1_000_000);
        amm.add_liquidity(&lp, &1_000_000, &1_000_000); // lp holds 999_000 shares, pool 1e6/1e6
        // removing 500_000 shares pays 500_000 of each; demanding 500_001 on side A must revert
        let supply_before = amm.lp_supply();
        assert!(amm.try_remove_liquidity_min(&lp, &500_000, &500_001, &0).is_err(), "floor A above payout -> Slippage");
        assert!(amm.try_remove_liquidity_min(&lp, &500_000, &0, &500_001).is_err(), "floor B above payout -> Slippage");
        assert_eq!(amm.lp_supply(), supply_before, "reverted remove left supply untouched");
        assert_eq!(amm.lp_balance(&lp), 999_000, "reverted remove burned no shares");
        // demanding exactly the payout succeeds
        assert_eq!(amm.remove_liquidity_min(&lp, &500_000, &500_000, &500_000), (500_000, 500_000), "floor == payout -> ok");
    }
}
