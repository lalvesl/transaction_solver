//! The command line interface.

use std::path::PathBuf;

use clap::Parser;

/// A toy payments engine.
///
/// Reads a CSV transaction log, applies it to client accounts, and writes the resulting
/// balances to standard output. Rejected records are reported on standard error.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// CSV file of transactions to process. Reads from standard input if omitted or `-`.
    pub input: Option<PathBuf>,
}
