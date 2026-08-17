//! A toy payments engine.
//!
//! A CSV transaction log is streamed through [`pipeline::Reader`] into one or more
//! [`Engine`]s, each keeping one [`Account`] per client. [`output`] renders the balances.
//!
//! There are two ways to drive it. [`pipeline::run`] applies every record to a single
//! engine in input order. [`parallel::run`] routes records to a fan of engines by client,
//! which is sound because no transaction can reach across clients.
//!
//! The design notes, and every assumption made where the specification was silent, are in
//! the README.

// SIMD here is the compiler's, not hand-written intrinsics: `route` is shaped so the
// auto-vectoriser can take it, which keeps the whole crate free of `unsafe`.
#![forbid(unsafe_code)]

pub mod account;
pub mod amount;
pub mod cli;
pub mod engine;
pub mod error;
pub mod output;
pub mod parallel;
pub mod pipeline;
pub mod record;
pub mod route;

pub use account::{Account, AccountRow};
pub use amount::Amount;
pub use engine::Engine;
pub use error::{FatalError, RecordError};
pub use record::Transaction;
pub use route::Router;
