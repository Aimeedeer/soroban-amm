use crate::admin::{check_admin, has_admin, require_admin, set_admin};
use crate::rewards::manager as rewards_manager;
use crate::rewards::storage as rewards_storage;
use crate::rewards::storage::get_pool_reward_config;
use crate::token::create_contract;
use crate::{pool, storage, token};
use cast::i128 as to_i128;
use num_integer::Roots;
use soroban_sdk::{
    contract, contractimpl, contractmeta, symbol_short, Address, BytesN, Env, IntoVal, Map, Symbol,
};

// Metadata that is added on to the WASM custom section
contractmeta!(
    key = "Description",
    val = "Constant product AMM with a .3% swap fee"
);

#[contract]
pub struct LiquidityPool;

pub trait LiquidityPoolTrait {
    // Sets the token contract addresses for this pool
    // todo: add reward_storage address to transfer from one place instead of per-pool balance
    fn initialize(
        e: Env,
        admin: Address,
        token_wasm_hash: BytesN<32>,
        token_a: Address,
        token_b: Address,
        reward_token: Address,
        reward_storage: Address,
    );

    // Returns the token contract address for the pool share token
    fn share_id(e: Env) -> Address;

    // Deposits token_a and token_b. Also mints pool shares for the "to" Identifier. The amount minted
    // is determined based on the difference between the reserves stored by this contract, and
    // the actual balance of token_a and token_b for this contract.
    fn deposit(
        e: Env,
        to: Address,
        desired_a: i128,
        min_a: i128,
        desired_b: i128,
        min_b: i128,
    ) -> (i128, i128);

    // If "buy_a" is true, the swap will buy token_a and sell token_b. This is flipped if "buy_a" is false.
    // "out" is the amount being bought, with in_max being a safety to make sure you receive at least that amount.
    // swap will transfer the selling token "to" to this contract, and then the contract will transfer the buying token to "to".
    fn swap(e: Env, to: Address, buy_a: bool, out: i128, in_max: i128) -> i128;
    fn estimate_swap_out(e: Env, buy_a: bool, out: i128) -> i128;

    // transfers share_amount of pool share tokens to this contract, burns all pools share tokens in this contracts, and sends the
    // corresponding amount of token_a and token_b to "to".
    // Returns amount of both tokens withdrawn
    fn withdraw(e: Env, to: Address, share_amount: i128, min_a: i128, min_b: i128) -> (i128, i128);

    fn get_rsrvs(e: Env) -> (i128, i128);

    fn version() -> u32;
    fn upgrade(e: Env, new_wasm_hash: BytesN<32>);
    fn set_rewards_config(e: Env, admin: Address, expired_at: u64, amount: i128);
    fn get_rewards_info(e: Env, user: Address) -> Map<Symbol, i128>;
    fn get_user_reward(e: Env, user: Address) -> i128;
    fn claim(e: Env, user: Address) -> i128;
}

#[contractimpl]
impl LiquidityPoolTrait for LiquidityPool {
    fn initialize(
        e: Env,
        admin: Address,
        token_wasm_hash: BytesN<32>,
        token_a: Address,
        token_b: Address,
        reward_token: Address,
        reward_storage: Address,
    ) {
        if has_admin(&e) {
            panic!("already initialized")
        }

        set_admin(&e, &admin);

        if token_a >= token_b {
            panic!("token_a must be less than token_b");
        }

        let share_contract = create_contract(&e, token_wasm_hash, &token_a, &token_b);
        token::Client::new(&e, &share_contract).initialize(
            &e.current_contract_address(),
            &7u32,
            &"Pool Share Token".into_val(&e),
            &"POOL".into_val(&e),
        );

        storage::put_token_a(&e, token_a);
        storage::put_token_b(&e, token_b);
        storage::put_reward_token(&e, reward_token);
        storage::put_reward_storage(&e, reward_storage);
        storage::put_token_share(&e, share_contract.try_into().unwrap());
        storage::put_reserve_a(&e, 0);
        storage::put_reserve_b(&e, 0);
        rewards_manager::set_reward_inv(&e, &Map::from_array(&e, [(0_u64, 0_u64)]));
        rewards_storage::set_pool_reward_config(
            &e,
            &rewards_storage::PoolRewardConfig {
                tps: 0,
                expired_at: 0,
            },
        );
        rewards_storage::set_pool_reward_data(
            &e,
            &rewards_storage::PoolRewardData {
                block: 0,
                accumulated: 0,
                last_time: 0,
            },
        );
    }

    fn share_id(e: Env) -> Address {
        storage::get_token_share(&e)
    }

    fn deposit(
        e: Env,
        to: Address,
        desired_a: i128,
        min_a: i128,
        desired_b: i128,
        min_b: i128,
    ) -> (i128, i128) {
        // Depositor needs to authorize the deposit
        to.require_auth();

        let (reserve_a, reserve_b) = (storage::get_reserve_a(&e), storage::get_reserve_b(&e));

        // Before actual changes were made to the pool, update total rewards data and refresh/initialize user reward
        let pool_data = rewards_manager::update_rewards_data(&e);
        rewards_manager::update_user_reward(&e, &pool_data, &to);
        rewards_storage::bump_user_reward_data(&e, &to);

        // Calculate deposit amounts
        let amounts =
            pool::get_deposit_amounts(desired_a, min_a, desired_b, min_b, reserve_a, reserve_b);

        let token_a_client = token::Client::new(&e, &storage::get_token_a(&e));
        let token_b_client = token::Client::new(&e, &storage::get_token_b(&e));

        token_a_client.transfer_from(
            &e.current_contract_address(),
            &to,
            &e.current_contract_address(),
            &amounts.0,
        );
        token_b_client.transfer_from(
            &e.current_contract_address(),
            &to,
            &e.current_contract_address(),
            &amounts.1,
        );

        // Now calculate how many new pool shares to mint
        let (balance_a, balance_b) = (token::get_balance_a(&e), token::get_balance_b(&e));
        let total_shares = token::get_total_shares(&e);

        let zero = 0;
        let new_total_shares = if reserve_a > zero && reserve_b > zero {
            let shares_a = (balance_a * total_shares) / reserve_a;
            let shares_b = (balance_b * total_shares) / reserve_b;
            shares_a.min(shares_b)
        } else {
            (balance_a * balance_b).sqrt()
        };

        token::mint_shares(&e, to, new_total_shares - total_shares);
        storage::put_reserve_a(&e, balance_a);
        storage::put_reserve_b(&e, balance_b);
        (amounts.0, amounts.1)
    }

    fn swap(e: Env, to: Address, buy_a: bool, out: i128, in_max: i128) -> i128 {
        to.require_auth();

        let (reserve_a, reserve_b) = (storage::get_reserve_a(&e), storage::get_reserve_b(&e));
        let (reserve_sell, reserve_buy) = if buy_a {
            (reserve_b, reserve_a)
        } else {
            (reserve_a, reserve_b)
        };

        // First calculate how much needs to be sold to buy amount out from the pool
        let n = reserve_sell * out * 1000;
        let d = (reserve_buy - out) * 997;
        let sell_amount = (n / d) + 1;
        if sell_amount > in_max {
            panic!("in amount is over max")
        }

        // Transfer the amount being sold to the contract
        let sell_token = if buy_a {
            storage::get_token_b(&e)
        } else {
            storage::get_token_a(&e)
        };
        let sell_token_client = token::Client::new(&e, &sell_token);
        sell_token_client.transfer_from(
            &e.current_contract_address(),
            &to,
            &e.current_contract_address(),
            &sell_amount,
        );

        let (balance_a, balance_b) = (token::get_balance_a(&e), token::get_balance_b(&e));

        // residue_numerator and residue_denominator are the amount that the invariant considers after
        // deducting the fee, scaled up by 1000 to avoid fractions
        let residue_numerator = 997;
        let residue_denominator = 1000;
        let zero = 0;

        let new_invariant_factor = |balance: i128, reserve: i128, out: i128| {
            let delta = balance - reserve - out;
            let adj_delta = if delta > zero {
                residue_numerator * delta
            } else {
                residue_denominator * delta
            };
            residue_denominator * reserve + adj_delta
        };

        let (out_a, out_b) = if buy_a { (out, 0) } else { (0, out) };

        let new_inv_a = new_invariant_factor(balance_a, reserve_a, out_a);
        let new_inv_b = new_invariant_factor(balance_b, reserve_b, out_b);
        let old_inv_a = residue_denominator * reserve_a;
        let old_inv_b = residue_denominator * reserve_b;

        if new_inv_a * new_inv_b < old_inv_a * old_inv_b {
            panic!("constant product invariant does not hold");
        }

        if buy_a {
            token::transfer_a(&e, to, out_a);
        } else {
            token::transfer_b(&e, to, out_b);
        }

        storage::put_reserve_a(&e, balance_a - out_a);
        storage::put_reserve_b(&e, balance_b - out_b);
        sell_amount
    }

    fn estimate_swap_out(e: Env, buy_a: bool, out: i128) -> i128 {
        let (reserve_a, reserve_b) = (storage::get_reserve_a(&e), storage::get_reserve_b(&e));
        let (reserve_sell, reserve_buy) = if buy_a {
            (reserve_b, reserve_a)
        } else {
            (reserve_a, reserve_b)
        };

        // Calculate how much needs to be sold to buy amount out from the pool
        let n = reserve_sell * out * 1000;
        let d = (reserve_buy - out) * 997;
        let sell_amount = (n / d) + 1;
        sell_amount
    }

    fn withdraw(e: Env, to: Address, share_amount: i128, min_a: i128, min_b: i128) -> (i128, i128) {
        to.require_auth();

        // Before actual changes were made to the pool, update total rewards data and refresh user reward
        let pool_data = rewards_manager::update_rewards_data(&e);
        rewards_manager::update_user_reward(&e, &pool_data, &to);
        rewards_storage::bump_user_reward_data(&e, &to);

        // First transfer the pool shares that need to be redeemed
        let share_token_client = token::Client::new(&e, &storage::get_token_share(&e));
        share_token_client.transfer_from(
            &e.current_contract_address(),
            &to,
            &e.current_contract_address(),
            &share_amount,
        );

        let (balance_a, balance_b) = (token::get_balance_a(&e), token::get_balance_b(&e));
        let balance_shares = token::get_balance_shares(&e);

        let total_shares = token::get_total_shares(&e);

        // Now calculate the withdraw amounts
        let out_a = (balance_a * balance_shares) / total_shares;
        let out_b = (balance_b * balance_shares) / total_shares;

        if out_a < min_a || out_b < min_b {
            panic!("min not satisfied");
        }

        token::burn_shares(&e, balance_shares);
        token::transfer_a(&e, to.clone(), out_a);
        token::transfer_b(&e, to, out_b);
        storage::put_reserve_a(&e, balance_a - out_a);
        storage::put_reserve_b(&e, balance_b - out_b);

        (out_a, out_b)
    }

    fn get_rsrvs(e: Env) -> (i128, i128) {
        (storage::get_reserve_a(&e), storage::get_reserve_b(&e))
    }

    fn version() -> u32 {
        1
    }

    fn upgrade(e: Env, new_wasm_hash: BytesN<32>) {
        require_admin(&e);
        e.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    fn set_rewards_config(
        e: Env,
        admin: Address,
        expired_at: u64, // timestamp
        amount: i128,    // value with 7 decimal places. example: 600_0000000
    ) {
        admin.require_auth();
        check_admin(&e, &admin);

        rewards_manager::update_rewards_data(&e);

        let config = rewards_storage::PoolRewardConfig {
            tps: amount / to_i128(expired_at - e.ledger().timestamp()),
            expired_at,
        };
        storage::bump_instance(&e);
        rewards_storage::set_pool_reward_config(&e, &config);
    }

    fn get_rewards_info(e: Env, user: Address) -> Map<Symbol, i128> {
        let config = get_pool_reward_config(&e);
        let pool_data = rewards_manager::update_rewards_data(&e);
        let user_data = rewards_manager::update_user_reward(&e, &pool_data, &user);
        let mut result = Map::new(&e);
        result.set(symbol_short!("tps"), to_i128(config.tps));
        result.set(symbol_short!("exp_at"), to_i128(config.expired_at));
        result.set(symbol_short!("acc"), to_i128(pool_data.accumulated));
        result.set(symbol_short!("last_time"), to_i128(pool_data.last_time));
        result.set(
            symbol_short!("pool_acc"),
            to_i128(user_data.pool_accumulated),
        );
        result.set(symbol_short!("block"), to_i128(pool_data.block));
        result.set(symbol_short!("usr_block"), to_i128(user_data.last_block));
        result.set(symbol_short!("to_claim"), to_i128(user_data.to_claim));
        result
    }

    fn get_user_reward(e: Env, user: Address) -> i128 {
        rewards_manager::get_amount_to_claim(&e, &user)
    }

    fn claim(e: Env, user: Address) -> i128 {
        let reward = rewards_manager::claim_reward(&e, &user);
        rewards_storage::bump_user_reward_data(&e, &user);
        reward
    }
}
