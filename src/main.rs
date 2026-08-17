use std::{
    error::Error,
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::Path,
    process::ExitCode,
};

use clap::Parser;
use transaction_solver::{cli::Cli, error::FatalError, output, pipeline, Engine};

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

    let input: Box<dyn Read> = match cli.input.as_deref() {
        Some(path) if path != Path::new("-") => {
            Box::new(File::open(path).map_err(|source| FatalError::Open {
                path: path.to_path_buf(),
                source,
            })?)
        }
        _ => Box::new(io::stdin()),
    };

    let mut engine = Engine::new();
    let mut diagnostics = BufWriter::new(io::stderr());

    // Rejected records are expected traffic from a partner, not a failure of the run, so
    // they are reported and counted rather than propagated.
    let rejected = pipeline::run(input, &mut engine, &mut diagnostics)?;
    if rejected > 0 {
        let _ = writeln!(diagnostics, "{rejected} record(s) rejected");
    }
    let _ = diagnostics.flush();

    output::write(&engine, BufWriter::new(io::stdout().lock()))
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
