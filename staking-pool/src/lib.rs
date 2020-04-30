use std::collections::HashMap;
use std::convert::TryInto;

use borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::Map;
use near_sdk::json_types::{Base58PublicKey, U128};
use near_sdk::{env, near_bindgen, AccountId, Balance, EpochHeight, Promise, PublicKey};

use uint::construct_uint;

const PING_GAS: u64 = 80_000_000_000_000;
const REFRESH_STAKE_GAS: u64 = 80_000_000_000_000;

construct_uint! {
    /// 256-bit unsigned integer.
    pub struct U256(4);
}

#[cfg(test)]
mod test_utils;

#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

#[derive(Debug)]
pub enum AccountStatus {
    Active,
    Inactive,
}

#[derive(BorshDeserialize, BorshSerialize, Debug, Clone)]
pub struct Account {
    pub unstaked: Balance,
    pub staked: Balance,
    pub unstaked_available_epoch_height: EpochHeight,
}

impl Account {
    pub fn new() -> Self {
        Self {
            unstaked: 0,
            staked: 0,
            unstaked_available_epoch_height: 0,
        }
    }
}

#[derive(Debug)]
pub struct WrappedAccount {
    pub account_id: AccountId,
    pub account: Account,
    pub status: AccountStatus,
}

#[derive(BorshDeserialize, BorshSerialize)]
pub struct EpochInfo {
    pub epoch_height: EpochHeight,
    pub stakes: HashMap<AccountId, Balance>,
}

impl EpochInfo {
    pub fn new(epoch_height: EpochHeight) -> Self {
        Self {
            epoch_height,
            stakes: HashMap::new(),
        }
    }
}

const EPOCHS_TOWARDS_REWARD: EpochHeight = 3;

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize)]
pub struct StakingContract {
    pub owner_id: AccountId,
    pub max_number_of_slots: u32,
    pub stake_public_key: PublicKey,
    pub last_locked_account_balance: Balance,
    pub last_account_balance: Balance,
    pub total_stake: Balance,
    pub epoch_infos: Vec<EpochInfo>,
    pub active_accounts: HashMap<AccountId, Account>,
    pub inactive_accounts: HashMap<AccountId, Account>,
    pub archived_accounts: Map<AccountId, Account>,
}

impl Default for StakingContract {
    fn default() -> Self {
        env::panic(b"Staking contract should be initialized before usage")
    }
}

#[near_bindgen]
impl StakingContract {
    /// Call to initialize the contract.
    /// Specify which account can change the staking key and the initial staking key with ED25519 curve.
    #[init]
    pub fn new(
        owner_id: AccountId,
        stake_public_key: Base58PublicKey,
        max_number_of_slots: u32,
    ) -> Self {
        assert!(!env::state_exists(), "Already initialized");
        Self {
            owner_id,
            stake_public_key: stake_public_key
                .try_into()
                .expect("invalid staking public key"),
            max_number_of_slots,
            last_locked_account_balance: 0,
            last_account_balance: env::account_balance(),
            total_stake: 0,
            epoch_infos: vec![EpochInfo::new(env::epoch_height())],
            active_accounts: HashMap::new(),
            inactive_accounts: HashMap::new(),
            archived_accounts: Map::new(b"u".to_vec()),
        }
    }

    /// Call to update state after epoch switched.
    pub fn ping(&mut self) {
        self.archive();
        // Checking if we need there are rewards to distribute.
        let epoch_height = env::epoch_height();
        if self.epoch_infos.last().unwrap().epoch_height == epoch_height {
            return;
        }
        let mut new_epoch_info = EpochInfo {
            epoch_height,
            stakes: self.epoch_infos.last().unwrap().stakes.clone(),
        };

        // Distributing the reward. Note, the reward can be 0.
        let total_balance =
            env::account_locked_balance() + env::account_balance() - env::attached_deposit();
        let last_total_balance = self.last_account_balance + self.last_locked_account_balance;

        let total_reward = total_balance.saturating_sub(last_total_balance);
        let total_staked_reward =
            env::account_locked_balance().saturating_sub(self.last_locked_account_balance);
        let total_unstaked_reward = total_reward.saturating_sub(total_staked_reward);

        let total_staked_reward = U256::from(total_staked_reward);
        let total_unstaked_reward = U256::from(total_unstaked_reward);

        let mut rewarded_accounts = HashMap::new();
        let mut total_stake: Balance = 0;
        for epoch_info in std::mem::take(&mut self.epoch_infos) {
            if epoch_info.epoch_height + EPOCHS_TOWARDS_REWARD <= epoch_height {
                for (account_id, stake) in epoch_info.stakes.into_iter() {
                    *rewarded_accounts.entry(account_id).or_insert(0) += stake;
                    total_stake += stake;
                }
            } else {
                self.epoch_infos.push(epoch_info);
            }
        }

        // The total stake can also be 0 if there were no staking.
        if total_stake > 0 {
            let total_stake = U256::from(total_stake);
            for (account_id, stake) in rewarded_accounts {
                let staked_reward =
                    (total_staked_reward * U256::from(stake) / total_stake).as_u128();
                let unstaked_reward =
                    (total_unstaked_reward * U256::from(stake) / total_stake).as_u128();
                {
                    let account = self.active_accounts.get_mut(&account_id).unwrap();
                    account.staked += staked_reward;
                    account.unstaked += unstaked_reward;
                }
                if let Some(stake) = new_epoch_info.stakes.get_mut(&account_id) {
                    *stake += staked_reward;
                    self.total_stake += staked_reward;
                }
            }
        }

        self.epoch_infos.push(new_epoch_info);

        // Moving inactive accounts towards archiving.
        for (account_id, account) in std::mem::take(&mut self.active_accounts) {
            if self
                .epoch_infos
                .iter()
                .any(|epoch_info| epoch_info.stakes.contains_key(&account_id))
            {
                self.active_accounts.insert(account_id, account);
            } else {
                self.inactive_accounts.insert(account_id, account);
            }
        }

        self.last_locked_account_balance = env::account_locked_balance();
        self.last_account_balance = env::account_balance();
    }

    /// Call to deposit money.
    #[payable]
    pub fn deposit(&mut self) {
        self.ping();
        let account_id = env::predecessor_account_id();
        let mut wrapped_account = self.pull_account(account_id);
        wrapped_account.account.unstaked += env::attached_deposit();
        self.save_account(wrapped_account);
    }

    /// Withdraws the non staked balance for given account.
    pub fn withdraw(&mut self, amount: U128) {
        let amount = amount.into();
        assert!(amount > 0, "Withdrawal amount should be positive");
        self.ping();
        let account_id = env::predecessor_account_id();
        let mut wrapped_account = self.pull_account(account_id.clone());
        assert!(
            wrapped_account.account.unstaked >= amount,
            "Not enough unstaked balance to withdraw"
        );
        assert!(
            wrapped_account.account.unstaked_available_epoch_height <= env::epoch_height(),
            "The unstaked balance is not yet available due to unstaking delay"
        );
        wrapped_account.account.unstaked -= amount;
        self.save_account(wrapped_account);
        Promise::new(account_id).transfer(amount);
    }

    /// Stakes previously deposited money by given account on this account.
    pub fn stake(&mut self, amount: U128) -> Promise {
        let amount = amount.into();
        assert!(amount > 0, "Staking amount should be positive");
        self.ping();
        let account_id = env::predecessor_account_id();
        let mut wrapped_account = self.pull_account(account_id);
        assert!(
            wrapped_account.account.unstaked >= amount,
            "Not enough unstaked balance to stake"
        );
        wrapped_account.account.unstaked -= amount;
        wrapped_account.account.staked += amount;

        self.update_stake(wrapped_account)
    }

    /// Request withdrawal for epoch + 2 by sending unstaking transaction for
    /// `current locked - (given account deposit + rewards)`
    pub fn unstake(&mut self, amount: U128) -> Promise {
        let amount = amount.into();
        assert!(amount > 0, "Unstaking amount should be positive");
        self.ping();
        let account_id = env::predecessor_account_id();
        let mut wrapped_account = self.pull_account(account_id);
        assert!(
            wrapped_account.account.staked >= amount,
            "Not enough staked balance to unstake"
        );
        wrapped_account.account.staked -= amount;
        wrapped_account.account.unstaked += amount;
        wrapped_account.account.unstaked_available_epoch_height =
            env::epoch_height() + EPOCHS_TOWARDS_REWARD;

        self.update_stake(wrapped_account)
    }

    fn update_stake(&mut self, mut wrapped_account: WrappedAccount) -> Promise {
        let epoch_info = self.epoch_infos.last_mut().unwrap();
        if epoch_info.stakes.contains_key(&wrapped_account.account_id) {
            // Already staking something in the current epoch.
            if wrapped_account.account.staked == 0 {
                // Trying to unstake everything.
                self.total_stake -= epoch_info
                    .stakes
                    .remove(&wrapped_account.account_id)
                    .unwrap_or(0);
            } else {
                // Need to update the stake
                self.total_stake -= epoch_info
                    .stakes
                    .insert(
                        wrapped_account.account_id.clone(),
                        wrapped_account.account.staked,
                    )
                    .unwrap_or(0);
                self.total_stake += wrapped_account.account.staked;
            }
        } else {
            // Not staking in the current epoch yet.
            if wrapped_account.account.staked == 0 {
                // Don't need to update anything, since the account wasn't actively staking in the
                // current epoch.
            } else if (epoch_info.stakes.len() as u32) < self.max_number_of_slots {
                // A slot is available
                epoch_info.stakes.insert(
                    wrapped_account.account_id.clone(),
                    wrapped_account.account.staked,
                );
                self.total_stake += wrapped_account.account.staked;
            } else {
                // No slots available. Need to check if we can kick out someone.
                let (account_id, smallest_stake) = epoch_info.stakes.iter().fold(
                    (None, 0),
                    |smallest_pair, (account_id, stake)| {
                        if smallest_pair.0.is_none() || *stake < smallest_pair.1 {
                            (Some(account_id.clone()), *stake)
                        } else {
                            smallest_pair
                        }
                    },
                );
                if smallest_stake < wrapped_account.account.staked {
                    epoch_info.stakes.remove(&account_id.unwrap());
                    self.total_stake -= smallest_stake;
                    epoch_info.stakes.insert(
                        wrapped_account.account_id.clone(),
                        wrapped_account.account.staked,
                    );
                    self.total_stake += wrapped_account.account.staked;
                } else {
                    // The current account stake is lower or equal to the current smallest stake.
                    // There are also no slots available, so the account can't take a slot.
                }
            }
        }

        if self
            .epoch_infos
            .iter()
            .any(|epoch_info| epoch_info.stakes.contains_key(&wrapped_account.account_id))
        {
            wrapped_account.status = AccountStatus::Active;
        } else {
            wrapped_account.status = AccountStatus::Inactive;
        }

        self.save_account(wrapped_account);

        Promise::new(env::current_account_id())
            .function_call(b"ping".to_vec(), b"{}".to_vec(), 0, PING_GAS)
            .stake(self.total_stake, self.stake_public_key.clone())
            .function_call(
                b"internal_after_stake".to_vec(),
                b"{}".to_vec(),
                0,
                REFRESH_STAKE_GAS,
            )
    }

    /// Private method to be called after stake action.
    pub fn internal_after_stake(&mut self) {
        assert_eq!(env::current_account_id(), env::predecessor_account_id());
        self.last_locked_account_balance = env::account_locked_balance();
    }

    /// Returns given account's unstaked balance.
    pub fn get_account_unstaked_balance(&self, account_id: AccountId) -> U128 {
        self.get_account(&account_id).unstaked.into()
    }

    /// Returns given account's staked balance.
    pub fn get_account_staked_balance(&self, account_id: AccountId) -> U128 {
        self.get_account(&account_id).staked.into()
    }

    pub fn get_account_total_balance(&self, account_id: AccountId) -> U128 {
        let account = self.get_account(&account_id);
        (account.staked + account.unstaked).into()
    }

    pub fn is_account_actively_staking(&self, account_id: AccountId) -> bool {
        self.epoch_infos
            .last()
            .unwrap()
            .stakes
            .contains_key(&account_id)
    }

    pub fn is_account_unstaked_balance_available(&self, account_id: AccountId) -> bool {
        self.get_account(&account_id)
            .unstaked_available_epoch_height
            <= env::epoch_height()
    }

    fn get_account(&self, account_id: &AccountId) -> Account {
        if let Some(account) = self.active_accounts.get(account_id) {
            account.clone()
        } else if let Some(account) = self.inactive_accounts.get(account_id) {
            account.clone()
        } else if let Some(account) = self.archived_accounts.get(account_id) {
            account
        } else {
            Account::new()
        }
    }

    fn pull_account(&mut self, account_id: AccountId) -> WrappedAccount {
        if let Some(account) = self.active_accounts.remove(&account_id) {
            WrappedAccount {
                account_id,
                account,
                status: AccountStatus::Active,
            }
        } else if let Some(account) = self.inactive_accounts.remove(&account_id) {
            WrappedAccount {
                account_id,
                account,
                status: AccountStatus::Inactive,
            }
        } else if let Some(account) = self.archived_accounts.remove(&account_id) {
            WrappedAccount {
                account_id,
                account,
                status: AccountStatus::Inactive,
            }
        } else {
            WrappedAccount {
                account_id,
                account: Account::new(),
                status: AccountStatus::Inactive,
            }
        }
    }

    fn save_account(&mut self, wrapped_account: WrappedAccount) {
        let WrappedAccount {
            account_id,
            account,
            status,
        } = wrapped_account;
        match status {
            AccountStatus::Active => {
                self.active_accounts.insert(account_id, account);
            }
            AccountStatus::Inactive if account.staked > 0 || account.unstaked > 0 => {
                self.archived_accounts.insert(&account_id, &account);
            }
            AccountStatus::Inactive => (),
        };
    }

    fn archive(&mut self) {
        if !self.inactive_accounts.is_empty() {
            let account_id = self.inactive_accounts.keys().next().unwrap().clone();
            let account = self.inactive_accounts.remove(&account_id).unwrap();
            self.archived_accounts.insert(&account_id, &account);
        }
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::{testing_env, MockedBlockchain};

    use crate::test_utils::*;

    use super::*;
    use std::convert::TryFrom;

    struct Emulator {
        pub contract: StakingContract,
        pub epoch_height: EpochHeight,
        pub amount: Balance,
        pub locked_amount: Balance,
    }

    impl Emulator {
        pub fn new(owner: String, stake_public_key: String, max_number_of_slots: u32) -> Self {
            testing_env!(VMContextBuilder::new()
                .current_account_id(owner.clone())
                .finish());
            Emulator {
                contract: StakingContract::new(
                    owner,
                    Base58PublicKey::try_from(stake_public_key).unwrap(),
                    max_number_of_slots,
                ),
                epoch_height: 0,
                amount: 0,
                locked_amount: 0,
            }
        }

        pub fn update_context(&mut self, predecessor_account_id: String, deposit: Balance) {
            testing_env!(VMContextBuilder::new()
                .current_account_id(staking())
                .predecessor_account_id(predecessor_account_id.clone())
                .signer_account_id(predecessor_account_id)
                .attached_deposit(deposit)
                .account_balance(self.amount)
                .account_locked_balance(self.locked_amount)
                .epoch_height(self.epoch_height)
                .finish());
            println!(
                "Deposit: {}, amount: {}, locked_amount: {}",
                deposit, self.amount, self.locked_amount
            );
        }

        pub fn simulate_stake_call(&mut self) {
            self.update_context(staking(), 0);
            let total_stake = self.contract.total_stake;
            // First function call action
            self.contract.ping();
            // Stake action
            if total_stake > self.locked_amount {
                let diff = total_stake - self.locked_amount;
                self.amount -= diff;
                self.locked_amount += diff;
            }
            // Second function call action
            self.update_context(staking(), 0);
            self.contract.internal_after_stake();
        }

        pub fn skip_epochs(&mut self, num: EpochHeight) {
            self.epoch_height += num;
            self.locked_amount = (self.locked_amount * 101 * u128::from(num)) / 100;
        }
    }

    #[test]
    fn test_deposit_withdraw() {
        let mut emulator = Emulator::new(
            "owner".to_string(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            10,
        );
        let deposit_amount = 1_000_000;
        emulator.update_context(bob(), deposit_amount);
        emulator.contract.deposit();
        emulator.amount += deposit_amount;
        emulator.update_context(bob(), 0);
        assert_eq!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            deposit_amount
        );
        emulator.contract.withdraw(deposit_amount.into());
        assert_eq!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            0u128
        );
    }

    #[test]
    fn test_stake_unstake() {
        let mut emulator = Emulator::new(
            "owner".to_string(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            10,
        );
        let deposit_amount = 1_000_000;
        emulator.update_context(bob(), deposit_amount);
        emulator.contract.deposit();
        emulator.amount += deposit_amount;
        emulator.update_context(bob(), 0);
        emulator.contract.stake(deposit_amount.into());
        emulator.simulate_stake_call();
        assert_eq!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount
        );
        // 10 epochs later, unstake half of the money.
        emulator.skip_epochs(10);
        // Overriding rewards
        emulator.locked_amount = deposit_amount + 10;
        emulator.update_context(bob(), 0);
        assert_eq!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount
        );
        emulator.contract.unstake((deposit_amount / 2).into());
        emulator.simulate_stake_call();
        assert_eq!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount / 2 + 10
        );
        assert_eq!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            deposit_amount / 2
        );
    }

    /// Test that two can delegate and then undelegate their funds and rewards at different time.
    #[test]
    fn test_two_delegates() {
        let mut emulator = Emulator::new(
            "owner".to_string(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            10,
        );
        emulator.update_context(alice(), 1_000_000);
        emulator.contract.deposit();
        emulator.amount += 1_000_000;
        emulator.update_context(alice(), 0);
        emulator.contract.stake(1_000_000.into());
        emulator.simulate_stake_call();
        emulator.skip_epochs(2);
        emulator.update_context(bob(), 1_000_000);
        emulator.contract.deposit();
        emulator.amount += 1_000_000;
        emulator.update_context(bob(), 0);
        emulator.contract.stake(1_000_000.into());
        emulator.simulate_stake_call();
        emulator.skip_epochs(2);
    }
}
