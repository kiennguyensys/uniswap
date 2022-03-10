use std::convert::TryInto;
use uint::construct_uint;
use near_sdk::borsh::{self, BorshSerialize, BorshDeserialize};
use near_sdk::collections::LookupMap;
use near_sdk::json_types::{ValidAccountId, U128};
use near_sdk::{
    env, ext_contract, near_bindgen, serde_json, AccountId, Balance, Gas, PanicOnDefault, Promise
};

const FEE_DIVISOR: u32 = 1_000;
const NO_DEPOSIT: Balance = 0;
const GAS_FOR_SWAP: Gas = 10_000_000_000_000;


construct_uint! {
    /// 256-bit unsigned integer.
    pub struct U256(4);
}

#[near_bindgen]
#[derive(BorshSerialize, BorshDeserialize, PanicOnDefault)]
struct Contract {
    token_account_id: AccountId,
    fee: u32,
    shares: LookupMap<AccountId, Balance>,
    shares_total_supply: Balance,
    near_amount: Balance,
    lp_token_amount: Balance
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new(token_account_id: ValidAccountId, fee: u32) -> Self {
        assert!(!env::state_exists(), "ERR_CONTRACT_IS_INITIALIZED");
        assert!(fee < FEE_DIVISOR, "ERR_FEE_TOO_LARGE");
        Self {
            token_account_id: token_account_id.into(),
            fee,
            shares: LookupMap::new(b"s".to_vec()),
            shares_total_supply: 0,
            near_amount: 0,
            lp_token_amount: 0
        }
    }

    pub fn add_liquidity(&mut self, sender_id: &AccountId, token_amount: U128) -> U128 {
        let near_amount = env::attached_deposit();
        assert!(near_amount > 0, "ERR_EMPTY_ATTACHED_DEPOSIT");

        if self.shares_total_supply > 0 {
            let expected_token_amount = near_amount * self.lp_token_amount / self.near_amount;
            assert!(expected_token_amount <= token_amount.into(), "ERR_NOT_ENOUGH_TOKEN");

            let liquidity_minted = near_amount * self.shares_total_supply / self.near_amount;
            add_to_collection(
                &mut self.shares, 
                sender_id, 
                liquidity_minted
            );

            self.shares_total_supply += liquidity_minted;
            self.near_amount += near_amount;
            self.lp_token_amount += expected_token_amount;
            expected_token_amount.into()
        } else {
            self.shares_total_supply = near_amount;
            self.near_amount = near_amount;
            self.lp_token_amount = token_amount.into();
            add_to_collection(&mut self.shares, sender_id, near_amount);
            token_amount
        }
    }

    pub fn remove_liquidity(&mut self, shares: Balance, min_near_amount: Balance, min_token_amount: Balance) -> Promise {
        let shares_amount = shares;
        assert!(shares_amount > 0 && self.shares_total_supply > 0, "ERR_EMPTY_SHARES");

        let near_amount = (U256::from(shares_amount) * U256::from(self.near_amount) / U256::from(self.shares_total_supply)).as_u128();
        let token_amount = (U256::from(shares_amount) * U256::from(self.lp_token_amount) / U256::from(self.shares_total_supply)).as_u128();
        assert!(near_amount >= min_near_amount && token_amount >= min_token_amount, "ERR_MIN_AMOUNT");

        let account_id = env::predecessor_account_id();
        let prev_amount = self.shares.get(&account_id).unwrap_or(0);
        assert!(prev_amount >= shares_amount, "ERR_NOT_ENOUGH_SHARES");

        if prev_amount == shares_amount {
            self.shares.remove(&account_id);
        } else {
            self.shares.insert(&account_id, &(prev_amount - shares_amount));
        }

        self.shares_total_supply -= shares_amount;
        self.near_amount -= near_amount;
        self.lp_token_amount -= token_amount;
        Promise::new(account_id.clone()).transfer(near_amount);

        ext_fungible_token::ft_transfer(
            account_id.try_into().unwrap(),
            U128(token_amount),
            None,
            &self.token_account_id,
            NO_DEPOSIT,
            env::prepaid_gas() - GAS_FOR_SWAP
        )
    }

    /*  Pricing between two reserves given input amount.
        a: input_amount, x: input_reserve, y: output_reserve
        (x+a) * (y-b) = k
        x * y = k
        xy + ya - xb - ab = k
        k + ya - xb - ab = k
        ya - xb - ab = 0
        ya = b(x+a)
        b = ya / (x+a)
        b * substract_fee / full_fee = ya * substract_fee / ((x + a) * full_fee)
    */
    pub fn get_input_price(&self, input_amount: Balance, input_reserve: Balance, output_reserve: Balance) -> Balance {
        assert!(input_reserve > 0 && output_reserve > 0, "ERR_EMPTY_RESERVE");

        let input_amount_with_fee = U256::from(input_amount) * U256::from(FEE_DIVISOR - self.fee);

        (input_amount_with_fee * U256::from(output_reserve)
        / (U256::from(input_reserve + input_amount) *  U256::from(FEE_DIVISOR)))
        .as_u128()
    }

    /*  Pricing between two reserves to return given output amount.
        a: output_amount, x: input_reserve, y: output_reserve
        (x+b) * (y-a) = k
        x*y + by -a * x - ab = k
        k + by - a*x - ab = k
        by - a*x - ab = 0
        by - ab  = a*x
        b(y - a) = a*x
        b = a * x / y - a
        b * full_fee / substract_fee = x * a * full_fee / (y - a) * substract_fee
    */

    pub fn get_output_price(&self, output_amount: Balance, input_reserve: Balance, output_reserve: Balance) -> Balance {
        assert!(input_reserve > 0 && output_reserve > 0, "ERR_EMPTY_RESERVE");

        (U256::from(input_reserve) * U256::from(output_amount) * U256::from(FEE_DIVISOR)
        / (U256::from(output_reserve - output_amount) * U256::from(FEE_DIVISOR - self.fee)))
        .as_u128()
    }

    pub fn get_near_to_token_price(&self, amount: Balance) -> Balance {
        self.get_output_price(amount, self.near_amount, self.lp_token_amount)
    }

    pub fn get_token_to_near_price(&self, amount: Balance) -> Balance {
        self.get_output_price(amount, self.lp_token_amount, self.near_amount)
    }

    #[payable]
    pub fn swap_near_to_token(&mut self, min_amount: Balance) -> Balance {
        let payed_amount = env::attached_deposit();
        let tokens_bought = self.get_input_price(payed_amount, self.near_amount, self.lp_token_amount);

        assert!(tokens_bought >= min_amount, "ERR_MIN_TOKENS_BOUGHT");

        self.near_amount += payed_amount;
        self.lp_token_amount -= tokens_bought;

        ext_fungible_token::ft_transfer(
            env::predecessor_account_id().try_into().unwrap(),
            U128::from(tokens_bought),
            None,
            &self.token_account_id,
            NO_DEPOSIT,
            env::prepaid_gas() - GAS_FOR_SWAP
        );
        tokens_bought
    }

    pub fn swap_token_to_near(&mut self, sender_id: AccountId, token_amount: Balance, min_near_amount: Balance) -> Promise {
        let near_bought = self.get_input_price(token_amount, self.lp_token_amount, self.near_amount);
        assert!(near_bought >= min_near_amount, "ERR_MIN_NEAR_AMOUNT");

        self.near_amount -= near_bought;
        self.lp_token_amount += token_amount;

        Promise::new(sender_id.clone()).transfer(near_bought)
    }

    pub fn shares_balance(&self, account_id: ValidAccountId) -> U128 {
        self.shares
            .get(account_id.as_ref())
            .unwrap_or(0)
            .into()
    }
}

#[ext_contract(ext_fungible_token)]
trait FungibleToken {
    fn ft_transfer(&mut self, receiver_id: ValidAccountId, amount: U128, memo: Option<String>);
}

trait FungibleTokenReceiver {
    fn ft_on_transfer(&mut self, sender_id: ValidAccountId, amount: U128, msg: String) -> U128;
}

impl FungibleTokenReceiver for Contract {
    fn ft_on_transfer(&mut self, sender_id: ValidAccountId, amount: U128, msg: String) -> U128 {
        assert_eq!(
            env::predecessor_account_id(),
            self.token_account_id,
            "ERR_WRONG_TOKEN"
        );
        if msg == "liquidity" {
            self.add_liquidity(sender_id.as_ref(), amount)
        } else {
            amount
        }
    }
}

pub fn add_to_collection(
    c: &mut LookupMap<AccountId, Balance>,
    account_id: &AccountId,
    amount: Balance
) {
    let prev_amount = c.get(account_id).unwrap_or(0);
    c.insert(account_id, &(prev_amount + amount));
}

fn main() {
    println!("Hello, uniswap!");
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, MockedBlockchain};

    use super::*;

    #[test]
    fn test_init_liquidity() {
        let one_near = 10u128.pow(24);
        let mut context = VMContextBuilder::new();
        context.predecessor_account_id(accounts(1));
        testing_env!(context.build());
        testing_env!(context.attached_deposit(5 * one_near).build());
        let mut contract = Contract::new(accounts(1), 3);
        contract.ft_on_transfer(accounts(1), (10 * one_near).into(), "liquidity".to_owned());

        // Test add_liquidity result
        let shares_amount: u128 = contract.shares_balance(accounts(1)).into();
        assert_eq!(shares_amount, 5 * one_near);
    }

    #[test] 
    fn test_swap() {
        let one_near = 10u128.pow(24);
        let mut context = VMContextBuilder::new();
        context.predecessor_account_id(accounts(1));
        testing_env!(context.build());
        testing_env!(context.attached_deposit(5 * one_near).build());
        let mut contract = Contract::new(accounts(1), 3);
        contract.ft_on_transfer(accounts(1), (10 * one_near).into(), "liquidity".to_owned());

        // Check output price
        let near_to_token = contract.get_near_to_token_price(one_near);
        assert_eq!(near_to_token, 557227237267357628440878);
        let token_to_near = contract.get_token_to_near_price(one_near);        
        assert_eq!(token_to_near, 2507522567703109327983951);

        // Check input price before swapping 3N for tokens
        let input_price = contract.get_input_price(3 * one_near, contract.near_amount, contract.lp_token_amount);
        /* Calculate input price
        (5 + 3) * (10 - b) = 5 * 10
        b = 10 - ( 50 / 8 ) = 3.75  */
        let expected_input_price = one_near / 100 * 375; // similar to * 3.75
        let expected_input_price_with_fee = 
            U256::from(expected_input_price) 
            * U256::from(FEE_DIVISOR - contract.fee) 
            / U256::from(FEE_DIVISOR);
        assert_eq!(input_price, expected_input_price_with_fee.as_u128());

        // Swap 3N for tokens, check that pool has 3N more and result tokens less.
        testing_env!(context.attached_deposit(3 * one_near).build());
        let result = contract.swap_near_to_token(1);
        assert_eq!(contract.near_amount, 8 * one_near);
        assert_eq!(contract.lp_token_amount, 10 * one_near - result);
    }

    #[test]
    fn test_remove_liquidity() {
        let one_near = 10u128.pow(24);
        let mut context = VMContextBuilder::new();
        context.predecessor_account_id(accounts(1));
        testing_env!(context.build());
        testing_env!(context.attached_deposit(5 * one_near).build());
        let mut contract = Contract::new(accounts(1), 3);
        contract.ft_on_transfer(accounts(1), (10 * one_near).into(), "liquidity".to_owned());
        
        // Withdraw all liquidity, check that nothing left.
        let shares_amount: u128 = contract.shares_balance(accounts(1)).into();
        contract.remove_liquidity(shares_amount, 1, 1);
        assert_eq!(contract.near_amount, 0);
        assert_eq!(contract.lp_token_amount, 0);
    }
}

