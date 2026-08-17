//! The input record: what a CSV row looks like, and the validated transaction it becomes.

use serde::Deserialize;

use crate::{amount::Amount, error::RecordError};

/// A row exactly as it appears in the input.
///
/// The string fields borrow from the reader's record buffer, so parsing a row allocates
/// nothing. `type` and `amount` are kept as text on purpose: validating them here, with
/// our own errors, gives far better diagnostics than a derived deserializer would.
#[derive(Debug, Deserialize)]
pub struct RawRecord<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub client: u16,
    pub tx: u32,
    /// Absent for dispute, resolve and chargeback rows, whether the column is empty or
    /// missing entirely.
    #[serde(default)]
    pub amount: Option<&'a str>,
}

/// A validated transaction, ready to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    Deposit {
        client: u16,
        tx: u32,
        amount: Amount,
    },
    Withdrawal {
        client: u16,
        tx: u32,
        amount: Amount,
    },
    Dispute {
        client: u16,
        tx: u32,
    },
    Resolve {
        client: u16,
        tx: u32,
    },
    Chargeback {
        client: u16,
        tx: u32,
    },
}

impl Transaction {
    /// The client this transaction belongs to.
    ///
    /// Every variant carries one, which is what makes the whole workload partitionable:
    /// no transaction can reach across clients ([D4](../README.md)), so routing by this
    /// value gives each shard a set of accounts nothing else can touch.
    pub fn client(&self) -> u16 {
        match *self {
            Self::Deposit { client, .. }
            | Self::Withdrawal { client, .. }
            | Self::Dispute { client, .. }
            | Self::Resolve { client, .. }
            | Self::Chargeback { client, .. } => client,
        }
    }

    /// Validates a raw row.
    ///
    /// Types are matched case-insensitively. The specification only ever writes them in
    /// lower case, but it also insists that whitespace be tolerated, and dropping a real
    /// movement of money over the capitalisation a partner used would be the worse error.
    pub fn from_raw(raw: &RawRecord<'_>) -> Result<Self, RecordError> {
        let RawRecord {
            kind,
            client,
            tx,
            amount,
        } = *raw;

        if kind.eq_ignore_ascii_case("deposit") {
            Ok(Self::Deposit {
                client,
                tx,
                amount: required_amount(amount, "deposit")?,
            })
        } else if kind.eq_ignore_ascii_case("withdrawal") {
            Ok(Self::Withdrawal {
                client,
                tx,
                amount: required_amount(amount, "withdrawal")?,
            })
        } else if kind.eq_ignore_ascii_case("dispute") {
            Ok(Self::Dispute { client, tx })
        } else if kind.eq_ignore_ascii_case("resolve") {
            Ok(Self::Resolve { client, tx })
        } else if kind.eq_ignore_ascii_case("chargeback") {
            Ok(Self::Chargeback { client, tx })
        } else {
            Err(RecordError::UnknownType(kind.to_owned()))
        }
    }
}

/// An amount is mandatory for deposits and withdrawals; the other types carry none, and
/// any value in the column is ignored.
fn required_amount(amount: Option<&str>, kind: &'static str) -> Result<Amount, RecordError> {
    match amount {
        Some(raw) if !raw.is_empty() => Amount::parse(raw),
        _ => Err(RecordError::MissingAmount(kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw<'a>(kind: &'a str, amount: Option<&'a str>) -> RawRecord<'a> {
        RawRecord {
            kind,
            client: 1,
            tx: 7,
            amount,
        }
    }

    #[test]
    fn parses_every_type() {
        let cases: [(&str, Option<&str>, Transaction); 5] = [
            (
                "deposit",
                Some("1.5"),
                Transaction::Deposit {
                    client: 1,
                    tx: 7,
                    amount: Amount::parse("1.5").unwrap(),
                },
            ),
            (
                "withdrawal",
                Some("1.5"),
                Transaction::Withdrawal {
                    client: 1,
                    tx: 7,
                    amount: Amount::parse("1.5").unwrap(),
                },
            ),
            ("dispute", None, Transaction::Dispute { client: 1, tx: 7 }),
            ("resolve", None, Transaction::Resolve { client: 1, tx: 7 }),
            (
                "chargeback",
                None,
                Transaction::Chargeback { client: 1, tx: 7 },
            ),
        ];

        for (kind, amount, expected) in cases {
            assert_eq!(Transaction::from_raw(&raw(kind, amount)).unwrap(), expected);
        }
    }

    #[test]
    fn type_matching_is_case_insensitive() {
        assert!(Transaction::from_raw(&raw("DEPOSIT", Some("1.0"))).is_ok());
        assert!(Transaction::from_raw(&raw("Dispute", None)).is_ok());
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(matches!(
            Transaction::from_raw(&raw("transfer", Some("1.0"))),
            Err(RecordError::UnknownType(_))
        ));
    }

    #[test]
    fn value_movements_require_an_amount() {
        for kind in ["deposit", "withdrawal"] {
            assert!(matches!(
                Transaction::from_raw(&raw(kind, None)),
                Err(RecordError::MissingAmount(_))
            ));
            assert!(matches!(
                Transaction::from_raw(&raw(kind, Some(""))),
                Err(RecordError::MissingAmount(_))
            ));
        }
    }

    #[test]
    fn references_ignore_a_stray_amount() {
        assert_eq!(
            Transaction::from_raw(&raw("dispute", Some("9.9"))).unwrap(),
            Transaction::Dispute { client: 1, tx: 7 }
        );
    }
}
