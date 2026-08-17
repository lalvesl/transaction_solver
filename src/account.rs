//! A single client's account: its balances, its disputable history, and every rule that
//! moves money between the two buckets.

use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::{amount::Amount, error::RecordError};

/// What a retained transaction is, and whether it is currently under dispute.
///
/// The withdrawal states only exist when the `dispute-withdraw` feature is enabled; in
/// the default build a withdrawal is never retained, so it cannot be referenced at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    Deposit,
    DisputedDeposit,
    #[cfg(feature = "dispute-withdraw")]
    Withdrawal,
    #[cfg(feature = "dispute-withdraw")]
    DisputedWithdrawal,
}

#[derive(Debug, Clone, Copy)]
struct TxRecord {
    amount: Amount,
    state: TxState,
}

/// Keyed by a `u32` an untrusted partner chooses, so this map keeps the standard library's
/// randomly seeded `SipHash`: it is the one table in the engine an attacker could try to
/// fill with colliding keys. The client table cannot be attacked that way — `u16` bounds it
/// at 65,536 entries — and [`crate::route`] hashes client IDs with something far cheaper.
type TxHistory = HashMap<u32, TxRecord>;

/// A finished account, ready to be written.
///
/// Frozen accounts leave the engine the moment they freeze, so their row is produced there
/// rather than at the end of the run. A row owns its numbers and borrows nothing, which is
/// what lets one shard hand it to the writer task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountRow {
    pub client: u16,
    pub available: Decimal,
    pub held: Decimal,
    pub total: Decimal,
    pub locked: bool,
}

/// A client account.
///
/// The balances are private and `total` is derived rather than stored, so
/// `total = available + held` is not a rule anyone has to remember: it is the only shape
/// the type can take. Every mutation goes through [`Account::set_balances`], which is
/// also the single place that rejects an update it could not represent.
#[derive(Debug)]
pub struct Account {
    client: u16,
    available: Decimal,
    held: Decimal,
    txs: TxHistory,
}

impl Account {
    /// An account with no funds and no history.
    pub fn new(client: u16) -> Self {
        Self {
            client,
            available: Decimal::ZERO,
            held: Decimal::ZERO,
            txs: TxHistory::new(),
        }
    }

    /// This account as an output row.
    ///
    /// `locked` is passed in rather than read off the account: freezing is the engine's
    /// business now, because a frozen account is not kept here to be asked.
    pub fn row(&self, locked: bool) -> AccountRow {
        AccountRow {
            client: self.client,
            available: self.available,
            held: self.held,
            total: self.total(),
            locked,
        }
    }

    pub fn client(&self) -> u16 {
        self.client
    }

    pub fn available(&self) -> Decimal {
        self.available
    }

    pub fn held(&self) -> Decimal {
        self.held
    }

    /// Funds available plus funds held.
    pub fn total(&self) -> Decimal {
        // Saturating rather than checked: `set_balances` refuses any update whose sum is
        // unrepresentable, so this cannot actually saturate, and a total is still wanted
        // for output even if that invariant were ever broken.
        debug_assert!(self.available.checked_add(self.held).is_some());
        self.available.saturating_add(self.held)
    }

    /// Credits the account and retains the transaction for later dispute.
    pub fn deposit(&mut self, tx: u32, amount: Amount) -> Result<(), RecordError> {
        if self.txs.contains_key(&tx) {
            return Err(RecordError::DuplicateTx {
                client: self.client,
                tx,
            });
        }

        let available = self.add(self.available, amount.value())?;
        self.set_balances(available, self.held)?;
        self.txs.insert(
            tx,
            TxRecord {
                amount,
                state: TxState::Deposit,
            },
        );
        Ok(())
    }

    /// Debits the account, if it can cover the amount.
    pub fn withdraw(&mut self, tx: u32, amount: Amount) -> Result<(), RecordError> {
        #[cfg(feature = "dispute-withdraw")]
        if self.txs.contains_key(&tx) {
            return Err(RecordError::DuplicateTx {
                client: self.client,
                tx,
            });
        }

        if self.available < amount.value() {
            return Err(RecordError::InsufficientFunds {
                client: self.client,
                available: self.available.to_string(),
                requested: amount.to_string(),
            });
        }

        let available = self.sub(self.available, amount.value())?;
        self.set_balances(available, self.held)?;

        #[cfg(feature = "dispute-withdraw")]
        self.txs.insert(
            tx,
            TxRecord {
                amount,
                state: TxState::Withdrawal,
            },
        );
        #[cfg(not(feature = "dispute-withdraw"))]
        let _ = tx; // Withdrawals are not disputable, so nothing is retained.

        Ok(())
    }

    /// Holds the funds behind a transaction while a claim is investigated.
    ///
    /// Disputing a deposit moves funds out of `available` and into `held`. Disputing a
    /// withdrawal instead adds a provisional credit: the institution fronts the amount
    /// into `held`, so the total rises while the claim is open.
    pub fn dispute(&mut self, tx: u32) -> Result<(), RecordError> {
        let client = self.client;
        let record = self.record(tx)?;
        let amount = record.amount.value();

        let (available, held, state) = match record.state {
            TxState::Deposit => (
                self.sub(self.available, amount)?,
                self.add(self.held, amount)?,
                TxState::DisputedDeposit,
            ),
            #[cfg(feature = "dispute-withdraw")]
            TxState::Withdrawal => (
                self.available,
                self.add(self.held, amount)?,
                TxState::DisputedWithdrawal,
            ),
            TxState::DisputedDeposit => return Err(RecordError::AlreadyDisputed { client, tx }),
            #[cfg(feature = "dispute-withdraw")]
            TxState::DisputedWithdrawal => return Err(RecordError::AlreadyDisputed { client, tx }),
        };

        self.set_balances(available, held)?;
        self.record_mut(tx)?.state = state;
        Ok(())
    }

    /// Ends a dispute without reversing anything.
    ///
    /// For a deposit the held funds go back to `available`. For a withdrawal the claim
    /// was denied, so the provisional credit is withdrawn again and the money stays gone.
    pub fn resolve(&mut self, tx: u32) -> Result<(), RecordError> {
        let client = self.client;
        let record = self.record(tx)?;
        let amount = record.amount.value();

        let (available, held) = match record.state {
            TxState::DisputedDeposit => (
                self.add(self.available, amount)?,
                self.sub(self.held, amount)?,
            ),
            #[cfg(feature = "dispute-withdraw")]
            TxState::DisputedWithdrawal => (self.available, self.sub(self.held, amount)?),
            TxState::Deposit => return Err(RecordError::NotDisputed { client, tx }),
            #[cfg(feature = "dispute-withdraw")]
            TxState::Withdrawal => return Err(RecordError::NotDisputed { client, tx }),
        };

        self.set_balances(available, held)?;
        // Resolved is terminal, so the record is dropped rather than kept forever.
        self.txs.remove(&tx);
        Ok(())
    }

    /// Reverses a transaction and freezes the account.
    ///
    /// Reversing a deposit takes the held funds away for good. Reversing a withdrawal
    /// releases the provisional credit to the client instead, leaving the total unchanged.
    pub fn chargeback(&mut self, tx: u32) -> Result<(), RecordError> {
        let client = self.client;
        let record = self.record(tx)?;
        let amount = record.amount.value();

        let (available, held) = match record.state {
            TxState::DisputedDeposit => (self.available, self.sub(self.held, amount)?),
            #[cfg(feature = "dispute-withdraw")]
            TxState::DisputedWithdrawal => (
                self.add(self.available, amount)?,
                self.sub(self.held, amount)?,
            ),
            TxState::Deposit => return Err(RecordError::NotDisputed { client, tx }),
            #[cfg(feature = "dispute-withdraw")]
            TxState::Withdrawal => return Err(RecordError::NotDisputed { client, tx }),
        };

        self.set_balances(available, held)?;
        // The caller freezes the client and drops this account, which releases the
        // history with it. Nothing here can ever be read again.
        Ok(())
    }

    fn record(&mut self, tx: u32) -> Result<TxRecord, RecordError> {
        let client = self.client;
        self.txs
            .get(&tx)
            .copied()
            .ok_or(RecordError::UnknownTx { client, tx })
    }

    fn record_mut(&mut self, tx: u32) -> Result<&mut TxRecord, RecordError> {
        let client = self.client;
        self.txs
            .get_mut(&tx)
            .ok_or(RecordError::UnknownTx { client, tx })
    }

    /// The only way to change a balance.
    fn set_balances(&mut self, available: Decimal, held: Decimal) -> Result<(), RecordError> {
        if available.checked_add(held).is_none() {
            return Err(RecordError::Overflow(self.client));
        }
        self.available = available;
        self.held = held;
        Ok(())
    }

    fn add(&self, a: Decimal, b: Decimal) -> Result<Decimal, RecordError> {
        a.checked_add(b).ok_or(RecordError::Overflow(self.client))
    }

    fn sub(&self, a: Decimal, b: Decimal) -> Result<Decimal, RecordError> {
        a.checked_sub(b).ok_or(RecordError::Overflow(self.client))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn amount(raw: &str) -> Amount {
        Amount::parse(raw).expect("test amount should be valid")
    }

    fn dec(raw: &str) -> Decimal {
        Decimal::from_str(raw).expect("test decimal should be valid")
    }

    /// Asserts the balances and that the invariant still holds.
    #[track_caller]
    fn assert_balances(account: &Account, available: &str, held: &str) {
        assert_eq!(account.available(), dec(available), "available");
        assert_eq!(account.held(), dec(held), "held");
        assert_eq!(
            account.total(),
            account.available() + account.held(),
            "total must equal available + held"
        );
    }

    fn funded(client: u16, tx: u32, raw: &str) -> Account {
        let mut account = Account::new(client);
        account.deposit(tx, amount(raw)).expect("deposit");
        account
    }

    #[test]
    fn deposit_credits_available_and_total() {
        let account = funded(1, 1, "1.5");
        assert_balances(&account, "1.5", "0");
        assert_eq!(account.total(), dec("1.5"));
    }

    #[test]
    fn duplicate_tx_is_rejected_without_touching_the_balance() {
        let mut account = funded(1, 1, "1.5");
        assert!(matches!(
            account.deposit(1, amount("99")),
            Err(RecordError::DuplicateTx { .. })
        ));
        assert_balances(&account, "1.5", "0");
    }

    #[test]
    fn withdrawal_debits_available() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("1.5")).expect("withdrawal");
        assert_balances(&account, "3.5", "0");
    }

    #[test]
    fn withdrawal_beyond_available_is_rejected() {
        let mut account = funded(1, 1, "1");
        assert!(matches!(
            account.withdraw(2, amount("1.0001")),
            Err(RecordError::InsufficientFunds { .. })
        ));
        assert_balances(&account, "1", "0");
    }

    #[test]
    fn dispute_holds_funds_leaving_total_unchanged() {
        let mut account = funded(1, 1, "5");
        account.dispute(1).expect("dispute");
        assert_balances(&account, "0", "5");
        assert_eq!(account.total(), dec("5"));
    }

    #[test]
    fn resolve_returns_held_funds() {
        let mut account = funded(1, 1, "5");
        account.dispute(1).expect("dispute");
        account.resolve(1).expect("resolve");
        assert_balances(&account, "5", "0");
    }

    /// Freezing itself belongs to the engine, which evicts the account: see
    /// `engine::tests::a_chargeback_evicts_the_account`. What the account owes is the
    /// reversal.
    #[test]
    fn chargeback_removes_the_funds() {
        let mut account = funded(1, 1, "5");
        account.dispute(1).expect("dispute");
        account.chargeback(1).expect("chargeback");
        assert_balances(&account, "0", "0");
    }

    #[test]
    fn a_reversed_deposit_can_leave_available_negative() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("5")).expect("withdrawal");
        account.dispute(1).expect("dispute");
        assert_balances(&account, "-5", "5");
        assert_eq!(account.total(), dec("0"));

        account.chargeback(1).expect("chargeback");
        assert_balances(&account, "-5", "0");
        assert_eq!(account.total(), dec("-5"));
    }

    // D3 — a frozen client rejects everything — is now enforced one level up, because a
    // frozen account is not kept here to reject anything. See
    // `engine::tests::a_frozen_client_rejects_everything`.

    #[test]
    fn funds_held_when_the_account_locks_stay_held() {
        let mut account = funded(1, 1, "5");
        account.deposit(2, amount("3")).expect("second deposit");
        account.dispute(2).expect("dispute the second deposit");
        account.dispute(1).expect("dispute the first deposit");
        account.chargeback(1).expect("chargeback");

        // The second dispute can never be settled now, and its funds remain held.
        assert_balances(&account, "0", "3");
    }

    #[test]
    fn unknown_transactions_cannot_be_referenced() {
        let mut account = funded(1, 1, "5");
        for result in [
            account.dispute(99),
            account.resolve(99),
            account.chargeback(99),
        ] {
            assert!(matches!(result, Err(RecordError::UnknownTx { tx: 99, .. })));
        }
    }

    #[test]
    fn settling_requires_an_open_dispute() {
        let mut account = funded(1, 1, "5");
        assert!(matches!(
            account.resolve(1),
            Err(RecordError::NotDisputed { .. })
        ));
        assert!(matches!(
            account.chargeback(1),
            Err(RecordError::NotDisputed { .. })
        ));
    }

    #[test]
    fn a_transaction_cannot_be_disputed_twice() {
        let mut account = funded(1, 1, "5");
        account.dispute(1).expect("dispute");
        assert!(matches!(
            account.dispute(1),
            Err(RecordError::AlreadyDisputed { .. })
        ));
        assert_balances(&account, "0", "5");
    }

    #[test]
    fn a_resolved_transaction_cannot_be_disputed_again() {
        let mut account = funded(1, 1, "5");
        account.dispute(1).expect("dispute");
        account.resolve(1).expect("resolve");
        assert!(matches!(
            account.dispute(1),
            Err(RecordError::UnknownTx { .. })
        ));
        assert_balances(&account, "5", "0");
    }

    #[test]
    fn overflow_is_reported_rather_than_panicking() {
        let mut account = Account::new(1);
        account
            .deposit(1, amount("79228162514264337593543950335"))
            .expect("largest representable deposit");
        assert!(matches!(
            account.deposit(2, amount("1")),
            Err(RecordError::Overflow(1))
        ));
        assert_eq!(account.available(), Decimal::MAX);
    }

    #[cfg(not(feature = "dispute-withdraw"))]
    #[test]
    fn withdrawals_are_not_disputable_by_default() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("5")).expect("withdrawal");
        assert!(matches!(
            account.dispute(2),
            Err(RecordError::UnknownTx { tx: 2, .. })
        ));
        assert_balances(&account, "0", "0");
    }

    #[cfg(feature = "dispute-withdraw")]
    #[test]
    fn a_disputed_withdrawal_is_a_provisional_credit() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("5")).expect("withdrawal");
        assert_balances(&account, "0", "0");

        account.dispute(2).expect("dispute the withdrawal");
        assert_balances(&account, "0", "5");
        assert_eq!(account.total(), dec("5"));
    }

    #[cfg(feature = "dispute-withdraw")]
    #[test]
    fn resolving_a_disputed_withdrawal_denies_the_claim() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("5")).expect("withdrawal");
        account.dispute(2).expect("dispute");
        account.resolve(2).expect("resolve");
        assert_balances(&account, "0", "0");
    }

    #[cfg(feature = "dispute-withdraw")]
    #[test]
    fn charging_back_a_withdrawal_returns_the_money() {
        let mut account = funded(1, 1, "5");
        account.withdraw(2, amount("5")).expect("withdrawal");
        account.dispute(2).expect("dispute");
        account.chargeback(2).expect("chargeback");
        assert_balances(&account, "5", "0");
        assert_eq!(account.total(), dec("5"));
    }
}
