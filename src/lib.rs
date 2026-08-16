//! A toy payments engine.
//!
//! Input is validated at the boundary: an [`Amount`] or a [`Transaction`] can only be
//! constructed from a well-formed record, so nothing further in ever has to re-check a
//! value it was handed. [`Engine`] keeps one [`Account`] per client and applies those
//! transactions.
//!
//! The design notes, and every assumption made where the specification was silent, are in
//! the README.

#![forbid(unsafe_code)]

pub mod account;
pub mod amount;
pub mod engine;
pub mod error;
pub mod record;

pub use account::Account;
pub use amount::Amount;
pub use engine::Engine;
pub use error::{FatalError, RecordError};
pub use record::Transaction;
