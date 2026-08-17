//! Routes transactions to the account they belong to.

use std::collections::{HashMap, HashSet};

use crate::{
    account::{Account, AccountRow},
    error::RecordError,
    record::Transaction,
};

/// Every client account seen so far.
///
/// All state lives in this value: no globals, no statics, nothing shared. Two engines can
/// run side by side on separate inputs, which is what lets [`crate::parallel`] give each
/// shard its own and never synchronise between them.
#[derive(Debug)]
pub struct Engine {
    clients: HashMap<u16, Account>,
    /// Clients frozen by a chargeback. Their accounts are gone; only the ID remains, so
    /// that every later record naming them can still be refused.
    frozen: HashSet<u16>,
    /// Rows of accounts already evicted, waiting to be taken by the caller.
    evicted: Vec<AccountRow>,
}

impl Engine {
    /// A new engine, sized for the whole client key space up front.
    ///
    /// Client IDs are `u16`, so the table can never hold more than 65,536 entries.
    /// Allocating it once costs a fixed few megabytes and buys a run with no rehashing:
    /// no growth spikes, and no per-record chance of paying for a table copy.
    pub fn new() -> Self {
        Self::with_capacity(u16::MAX as usize)
    }

    /// A new engine sized for `clients` accounts.
    ///
    /// A shard owns roughly `65,536 / shards` of the key space, so pre-sizing every shard
    /// for the whole of it would multiply the fixed cost by the shard count for no reason.
    pub fn with_capacity(clients: usize) -> Self {
        Self {
            clients: HashMap::with_capacity(clients),
            frozen: HashSet::new(),
            evicted: Vec::new(),
        }
    }

    /// Applies one transaction.
    pub fn apply(&mut self, transaction: Transaction) -> Result<(), RecordError> {
        // A frozen account is refused before anything else looks at it. This is the only
        // place that still knows the client existed: the account itself is long gone.
        let client = transaction.client();
        if self.frozen.contains(&client) {
            return Err(RecordError::AccountLocked(client));
        }

        match transaction {
            Transaction::Deposit { client, tx, amount } => self.opened(client).deposit(tx, amount),
            Transaction::Withdrawal { client, tx, amount } => {
                self.opened(client).withdraw(tx, amount)
            }
            Transaction::Dispute { client, tx } => self.existing(client, tx)?.dispute(tx),
            Transaction::Resolve { client, tx } => self.existing(client, tx)?.resolve(tx),
            Transaction::Chargeback { client, tx } => {
                self.existing(client, tx)?.chargeback(tx)?;
                self.freeze(client);
                Ok(())
            }
        }
    }

    /// Rows for accounts frozen since this was last called.
    ///
    /// Draining rather than reading: a frozen account's row is final, so the caller can
    /// write it and forget it. This is what keeps peak memory tied to the accounts still
    /// in play rather than to every account the run has ever touched.
    pub fn take_evicted(&mut self) -> std::vec::Drain<'_, AccountRow> {
        self.evicted.drain(..)
    }

    /// Every row the engine still owes: whatever `take_evicted` has not taken, followed by
    /// the accounts still open. Leaves the engine empty.
    pub fn drain_rows(&mut self) -> Vec<AccountRow> {
        let mut rows: Vec<AccountRow> = self.evicted.drain(..).collect();
        rows.extend(self.clients.drain().map(|(_, account)| account.row(false)));
        rows
    }

    /// True once a chargeback has frozen this client.
    pub fn is_frozen(&self, client: u16) -> bool {
        self.frozen.contains(&client)
    }

    /// Freezes a client and evicts its account.
    ///
    /// A chargeback is terminal for the whole account, so nothing it holds can be read
    /// again: the account leaves the table along with its entire transaction history, and
    /// what stays behind is two bytes in a set. On the `settled` benchmark profile the
    /// history is the memory — this is the only path that returns it outright.
    fn freeze(&mut self, client: u16) {
        if let Some(account) = self.clients.remove(&client) {
            self.evicted.push(account.row(true));
        }
        self.frozen.insert(client);
    }

    /// Every account, in no particular order.
    pub fn accounts(&self) -> impl Iterator<Item = &Account> {
        self.clients.values()
    }

    /// One account, if the client exists.
    pub fn account(&self, client: u16) -> Option<&Account> {
        self.clients.get(&client)
    }

    /// A deposit or withdrawal opens an account for a client we have not seen before.
    fn opened(&mut self, client: u16) -> &mut Account {
        self.clients
            .entry(client)
            .or_insert_with(|| Account::new(client))
    }

    /// A dispute, resolve or chargeback resolves against an existing client only. A
    /// record that is about to be rejected must not bring an account into existence.
    fn existing(&mut self, client: u16, tx: u32) -> Result<&mut Account, RecordError> {
        self.clients
            .get_mut(&client)
            .ok_or(RecordError::UnknownTx { client, tx })
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rust_decimal::Decimal;

    use super::*;
    use crate::amount::Amount;

    fn deposit(client: u16, tx: u32, raw: &str) -> Transaction {
        Transaction::Deposit {
            client,
            tx,
            amount: Amount::parse(raw).expect("valid amount"),
        }
    }

    fn withdrawal(client: u16, tx: u32, raw: &str) -> Transaction {
        Transaction::Withdrawal {
            client,
            tx,
            amount: Amount::parse(raw).expect("valid amount"),
        }
    }

    #[test]
    fn clients_are_independent() {
        let mut engine = Engine::new();
        engine.apply(deposit(1, 1, "1.0")).expect("deposit");
        engine.apply(deposit(2, 2, "2.0")).expect("deposit");
        engine.apply(deposit(1, 3, "2.0")).expect("deposit");
        engine.apply(withdrawal(1, 4, "1.5")).expect("withdrawal");

        assert_eq!(
            engine.account(1).expect("client 1").available(),
            Decimal::from_str("1.5").expect("decimal")
        );
        assert_eq!(
            engine.account(2).expect("client 2").available(),
            Decimal::from_str("2.0").expect("decimal")
        );
    }

    #[test]
    fn a_deposit_opens_an_account() {
        let mut engine = Engine::new();
        engine.apply(deposit(7, 1, "1.0")).expect("deposit");
        assert!(engine.account(7).is_some());
    }

    #[test]
    fn a_failed_withdrawal_still_opens_an_account() {
        let mut engine = Engine::new();
        assert!(engine.apply(withdrawal(7, 1, "1.0")).is_err());

        let account = engine
            .account(7)
            .expect("the client was named by a valid record");
        assert_eq!(account.available(), Decimal::ZERO);
    }

    #[test]
    fn a_reference_to_an_unknown_client_opens_nothing() {
        let mut engine = Engine::new();
        for transaction in [
            Transaction::Dispute { client: 7, tx: 1 },
            Transaction::Resolve { client: 7, tx: 1 },
            Transaction::Chargeback { client: 7, tx: 1 },
        ] {
            assert!(matches!(
                engine.apply(transaction),
                Err(RecordError::UnknownTx { client: 7, tx: 1 })
            ));
        }
        assert!(engine.account(7).is_none());
        assert_eq!(engine.accounts().count(), 0);
    }

    fn chargeback(client: u16, tx: u32) -> Transaction {
        Transaction::Chargeback { client, tx }
    }

    /// Drives a client to a chargeback and returns the engine holding the aftermath.
    fn frozen(client: u16) -> Engine {
        let mut engine = Engine::new();
        engine.apply(deposit(client, 1, "5.0")).expect("deposit");
        engine
            .apply(Transaction::Dispute { client, tx: 1 })
            .expect("dispute");
        engine.apply(chargeback(client, 1)).expect("chargeback");
        engine
    }

    #[test]
    fn a_chargeback_evicts_the_account() {
        let mut engine = frozen(1);

        assert!(engine.is_frozen(1));
        assert!(
            engine.account(1).is_none(),
            "the account and its history should be gone, not merely flagged"
        );

        let evicted: Vec<_> = engine.take_evicted().collect();
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].client, 1);
        assert!(evicted[0].locked);
        assert_eq!(evicted[0].total, Decimal::ZERO);
    }

    /// D3: the freeze covers every kind of record, not just the money-moving ones.
    #[test]
    fn a_frozen_client_rejects_everything() {
        let mut engine = frozen(1);

        for transaction in [
            deposit(1, 2, "1.0"),
            withdrawal(1, 3, "1.0"),
            Transaction::Dispute { client: 1, tx: 1 },
            Transaction::Resolve { client: 1, tx: 1 },
            chargeback(1, 1),
        ] {
            assert!(
                matches!(
                    engine.apply(transaction),
                    Err(RecordError::AccountLocked(1))
                ),
                "a frozen client must refuse every record"
            );
        }

        assert!(
            engine.account(1).is_none(),
            "a refused record must not resurrect the account"
        );
    }

    /// A row is produced once and only once, whether the account froze mid-run or survived
    /// to the end.
    #[test]
    fn every_client_is_reported_exactly_once() {
        let mut engine = frozen(1);
        engine.apply(deposit(2, 9, "3.0")).expect("deposit");

        let taken: Vec<_> = engine.take_evicted().collect();
        let remaining = engine.drain_rows();

        let mut clients: Vec<u16> = taken
            .iter()
            .chain(remaining.iter())
            .map(|row| row.client)
            .collect();
        clients.sort_unstable();

        assert_eq!(clients, vec![1, 2]);
        assert!(
            engine.drain_rows().is_empty(),
            "draining twice must be empty"
        );
    }

    #[test]
    fn a_dispute_cannot_reach_another_clients_transaction() {
        let mut engine = Engine::new();
        engine.apply(deposit(1, 1, "5.0")).expect("deposit");
        engine.apply(deposit(2, 2, "5.0")).expect("deposit");

        // Client 2 names client 1's transaction. The per-client history makes this
        // unreachable, and the error does not admit that transaction 1 exists at all.
        assert!(matches!(
            engine.apply(Transaction::Dispute { client: 2, tx: 1 }),
            Err(RecordError::UnknownTx { client: 2, tx: 1 })
        ));
        assert_eq!(
            engine.account(1).expect("client 1").available(),
            Decimal::from_str("5.0").expect("decimal")
        );
    }
}
