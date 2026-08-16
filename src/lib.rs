//! A toy payments engine.
//!
//! A CSV transaction log is streamed through [`pipeline::run`] into an [`Engine`], which
//! keeps one [`Account`] per client. Once the input is exhausted, [`output::write`]
//! renders the final balances.
//!
//! The design notes, and every assumption made where the specification was silent, are in
//! the README.

#![forbid(unsafe_code)]

pub mod account;
pub mod amount;
pub mod cli;
pub mod engine;
pub mod error;
pub mod output;
pub mod pipeline;
pub mod record;

pub use account::Account;
pub use amount::Amount;
pub use engine::Engine;
pub use error::{FatalError, RecordError};
pub use record::Transaction;
