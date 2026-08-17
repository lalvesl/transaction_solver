//! Error types.
//!
//! Errors are split in two. A [`RecordError`] concerns a single input record: the record
//! is skipped, reported to stderr, and processing continues. A [`FatalError`] means the
//! run cannot continue at all.

use std::path::PathBuf;

use thiserror::Error;

/// A rejected input record.
///
/// Every case the specification tells us to "ignore" is one of these. They are reported
/// rather than silently dropped: a payments engine that discards records without a trace
/// is not auditable, and these are exactly the records that reveal a misbehaving partner.
#[derive(Debug, Error)]
pub enum RecordError {
    #[error("could not be parsed as a CSV record: {0}")]
    Malformed(#[source] csv::Error),

    #[error("unrecognised transaction type `{0}`")]
    UnknownType(String),

    #[error("a {0} requires an amount")]
    MissingAmount(&'static str),

    #[error("amount `{0}` is not a valid decimal number")]
    MalformedAmount(String),

    #[error("amount `{0}` is negative")]
    NegativeAmount(String),

    #[error("amount `{0}` has more than four decimal places")]
    ExcessPrecision(String),

    #[error("client {client} already has a transaction {tx}")]
    DuplicateTx { client: u16, tx: u32 },

    /// Covers all three ways a reference can fail to resolve: the transaction never
    /// existed, it belongs to a different client, or it has already been settled. They
    /// are deliberately indistinguishable, so that one client cannot probe for another
    /// client's transaction IDs.
    #[error("client {client} has no disputable transaction {tx}")]
    UnknownTx { client: u16, tx: u32 },

    #[error("transaction {tx} of client {client} is already under dispute")]
    AlreadyDisputed { client: u16, tx: u32 },

    #[error("transaction {tx} of client {client} is not under dispute")]
    NotDisputed { client: u16, tx: u32 },

    #[error("client {client} has {available} available, cannot withdraw {requested}")]
    InsufficientFunds {
        client: u16,
        available: String,
        requested: String,
    },

    #[error("client {0} is locked")]
    AccountLocked(u16),

    #[error("would overflow the balances of client {0}")]
    Overflow(u16),
}

/// A condition that ends the run.
#[derive(Debug, Error)]
pub enum FatalError {
    #[error("cannot open `{}`", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot read the input")]
    Read(#[source] csv::Error),

    #[error("cannot write the output")]
    Write(#[source] csv::Error),

    #[error("cannot start the worker runtime")]
    Runtime(#[source] std::io::Error),

    /// The reader thread unwound. Nothing downstream can be trusted to be complete, so
    /// this is reported rather than quietly returning a short result.
    #[error("the input reader stopped unexpectedly")]
    ReaderPanicked,
}
