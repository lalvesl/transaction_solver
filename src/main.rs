use std::{
    error::Error,
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::Path,
    process::ExitCode,
    thread,
};

use clap::Parser;
use transaction_solver::{cli::Cli, error::FatalError, parallel};

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<(), FatalError> {
    let cli = Cli::parse();

    let input: Box<dyn Read + Send> = match cli.input.as_deref() {
        Some(path) if path != Path::new("-") => {
            Box::new(File::open(path).map_err(|source| FatalError::Open {
                path: path.to_path_buf(),
                source,
            })?)
        }
        _ => Box::new(io::stdin()),
    };

    // `--shards` overrides the machine, which is what makes a benchmark comparable across
    // machines and what lets a test pin the layout.
    let shards = cli.shards.map(usize::from).unwrap_or_else(|| {
        parallel::shards_for(
            thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1),
        )
    });

    // Rejected records are expected traffic from a partner, not a failure of the run, so
    // the writer reports and counts them rather than propagating them.
    parallel::run(
        input,
        // `io::stdout()` rather than a lock guard: the guard is not `Send`, and the writer
        // task owns the stream outright anyway — it is the only thing that writes.
        Box::new(BufWriter::new(io::stdout())),
        Box::new(BufWriter::new(io::stderr())),
        shards,
    )
    .map(|_| ())
}

/// Prints an error and the chain of causes behind it.
fn report(error: &FatalError) {
    let mut stderr = io::stderr();
    let _ = writeln!(stderr, "error: {error}");

    let mut cause = error.source();
    while let Some(error) = cause {
        let _ = writeln!(stderr, "  caused by: {error}");
        cause = error.source();
    }
}
