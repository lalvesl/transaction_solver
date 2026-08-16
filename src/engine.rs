//! Routes transactions to the account they belong to.

use std::collections::HashMap;

use crate::{account::Account, error::RecordError, record::Transaction};

/// Every client account seen so far.
///
/// All state lives in this value: no globals, no statics, nothing shared. Two engines can
/// run side by side on separate inputs, which is what makes the sharding sketched in the
/// README a wiring change rather than a rewrite.
#[derive(Debug)]
pub struct Engine {
    clients: HashMap<u16, Account>,
}

impl Engine {
    /// A new engine, sized for the whole client key space up front.
    ///
    /// Client IDs are `u16`, so the table can never hold more than 65,536 entries.
    /// Allocating it once costs a fixed few megabytes and buys a run with no rehashing:
    /// no growth spikes, and no per-record chance of paying for a table copy.
    pub fn new() -> Self {
        Self {
            clients: HashMap::with_capacity(u16::MAX as usize),
        }
    }

    /// Applies one transaction.
    pub fn apply(&mut self, transaction: Transaction) -> Result<(), RecordError> {
        match transaction {
            Transaction::Deposit { client, tx, amount } => self.opened(client).deposit(tx, amount),
            Transaction::Withdrawal { client, tx, amount } => {
                self.opened(client).withdraw(tx, amount)
            }
            Transaction::Dispute { client, tx } => self.existing(client, tx)?.dispute(tx),
            Transaction::Resolve { client, tx } => self.existing(client, tx)?.resolve(tx),
            Transaction::Chargeback { client, tx } => self.existing(client, tx)?.chargeback(tx),
        }
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
