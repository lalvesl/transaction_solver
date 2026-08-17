//! Streaming the input into the engine.

use std::io::{Read, Write};

use csv::{ReaderBuilder, StringRecord, Trim};

use crate::{
    engine::Engine,
    error::{FatalError, RecordError},
    record::{RawRecord, Transaction},
};

/// A validated record and the input line it came from.
///
/// The line number travels with the transaction because the place that rejects a record is
/// no longer the place that read it: under [`crate::parallel`] the record has crossed a
/// channel into another thread by then, and a diagnostic that cannot name its line is
/// close to useless.
#[derive(Debug)]
pub struct Entry {
    pub line: u64,
    pub transaction: Transaction,
}

/// One step of the reader.
#[derive(Debug)]
pub enum Reading {
    /// A record that parsed and validated. It may still be refused by the engine.
    Ready(Entry),
    /// A record that never got as far as the engine.
    Rejected { line: u64, error: RecordError },
}

/// Pulls validated records off a CSV stream.
///
/// Records are read one at a time into a reusable buffer, so the input is never held in
/// memory: peak usage is a function of the engine's state, not of the file's size.
///
/// Splitting this out of [`run`] is what lets the same parser feed either a single engine
/// or a fan of sharded ones — the two differ in where a record goes, not in how it is read.
pub struct Reader<R: Read> {
    csv: csv::Reader<R>,
    headers: StringRecord,
    record: StringRecord,
}

impl<R: Read> Reader<R> {
    /// Opens the stream and reads its header row.
    pub fn new(reader: R) -> Result<Self, FatalError> {
        let mut csv = ReaderBuilder::new()
            // Whitespace around any field, including the header, must be tolerated.
            .trim(Trim::All)
            // A row may legitimately omit the trailing amount column entirely.
            .flexible(true)
            .from_reader(reader);

        let headers = csv.headers().map_err(FatalError::Read)?.clone();

        Ok(Self {
            csv,
            headers,
            record: StringRecord::new(),
        })
    }

    /// The next record, or `None` at the end of the stream.
    ///
    /// Only losing the stream itself is an error here. A record that cannot be parsed is
    /// the partner's mistake, like any other bad record, and comes back as
    /// [`Reading::Rejected`].
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Reading>, FatalError> {
        match self.csv.read_record(&mut self.record) {
            Ok(false) => Ok(None),
            Ok(true) => {
                let line = self.record.position().map_or(0, |position| position.line());

                Ok(Some(match self.parse() {
                    Ok(transaction) => Reading::Ready(Entry { line, transaction }),
                    Err(error) => Reading::Rejected { line, error },
                }))
            }
            Err(error) if error.is_io_error() => Err(FatalError::Read(error)),
            Err(error) => {
                let line = error.position().map_or(0, |position| position.line());
                Ok(Some(Reading::Rejected {
                    line,
                    error: RecordError::Malformed(error),
                }))
            }
        }
    }

    fn parse(&self) -> Result<Transaction, RecordError> {
        let raw: RawRecord<'_> = self
            .record
            .deserialize(Some(&self.headers))
            .map_err(RecordError::Malformed)?;

        Transaction::from_raw(&raw)
    }
}

/// Reads every record from `reader` and applies it to `engine`.
///
/// The single-threaded path: one engine, records applied in input order, diagnostics in
/// input order too. [`crate::parallel`] is the same work spread over shards.
///
/// Rejected records are reported to `diagnostics` and counted; only an I/O failure stops
/// the run.
pub fn run<R: Read, W: Write>(
    reader: R,
    engine: &mut Engine,
    diagnostics: &mut W,
) -> Result<u64, FatalError> {
    let mut reader = Reader::new(reader)?;
    let mut rejected = 0;

    while let Some(reading) = reader.next()? {
        match reading {
            Reading::Ready(entry) => {
                if let Err(error) = engine.apply(entry.transaction) {
                    report(diagnostics, entry.line, &error);
                    rejected += 1;
                }
            }
            Reading::Rejected { line, error } => {
                report(diagnostics, line, &error);
                rejected += 1;
            }
        }
    }

    Ok(rejected)
}

/// Reports a rejected record. A failing diagnostics stream must not take the run down
/// with it, so the result is deliberately discarded.
fn report<W: Write>(diagnostics: &mut W, line: u64, error: &RecordError) {
    let _ = writeln!(diagnostics, "line {line}: rejected, {error}");
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rust_decimal::Decimal;

    use super::*;

    struct Outcome {
        engine: Engine,
        rejected: u64,
        diagnostics: String,
    }

    fn process(input: &str) -> Outcome {
        let mut engine = Engine::new();
        let mut diagnostics = Vec::new();
        let rejected =
            run(input.as_bytes(), &mut engine, &mut diagnostics).expect("in-memory input");

        Outcome {
            engine,
            rejected,
            diagnostics: String::from_utf8(diagnostics).expect("diagnostics are UTF-8"),
        }
    }

    fn dec(raw: &str) -> Decimal {
        Decimal::from_str(raw).expect("valid decimal")
    }

    #[test]
    fn processes_the_specification_example() {
        let outcome = process(
            "type, client, tx, amount\n\
             deposit, 1, 1, 1.0\n\
             deposit, 2, 2, 2.0\n\
             deposit, 1, 3, 2.0\n\
             withdrawal, 1, 4, 1.5\n\
             withdrawal, 2, 5, 3.0\n",
        );

        // The last withdrawal is refused: client 2 only has 2.0.
        assert_eq!(outcome.rejected, 1);
        assert_eq!(
            outcome.engine.account(1).expect("client 1").available(),
            dec("1.5")
        );
        assert_eq!(
            outcome.engine.account(2).expect("client 2").available(),
            dec("2.0")
        );
    }

    #[test]
    fn tolerates_whitespace_missing_columns_and_crlf() {
        let outcome = process(
            "type , client , tx , amount\r\n\
             deposit ,  1 ,  1 ,  1.0000\r\n\
             dispute ,  1 ,  1 ,\r\n\
             resolve ,  1 ,  1\r\n",
        );

        assert_eq!(outcome.rejected, 0, "{}", outcome.diagnostics);
        let account = outcome.engine.account(1).expect("client 1");
        assert_eq!(account.available(), dec("1.0000"));
        assert_eq!(account.held(), Decimal::ZERO);
    }

    #[test]
    fn accepts_columns_in_any_order() {
        let outcome = process("amount,tx,client,type\n2.5,7,3,deposit\n");

        assert_eq!(outcome.rejected, 0, "{}", outcome.diagnostics);
        assert_eq!(
            outcome.engine.account(3).expect("client 3").available(),
            dec("2.5")
        );
    }

    #[test]
    fn a_bad_record_does_not_stop_the_run() {
        let outcome = process(
            "type,client,tx,amount\n\
             deposit,1,1,1.0\n\
             deposit,notanumber,2,1.0\n\
             transfer,1,3,1.0\n\
             deposit,1,4,1.00001\n\
             deposit,1,5,-1.0\n\
             withdrawal,1,6,\n\
             deposit,1,7,1.0\n",
        );

        assert_eq!(outcome.rejected, 5);
        assert_eq!(
            outcome.engine.account(1).expect("client 1").available(),
            dec("2.0")
        );

        for expected in [
            "line 3",
            "line 4: rejected, unrecognised transaction type `transfer`",
            "line 5",
            "line 6",
            "line 7",
        ] {
            assert!(
                outcome.diagnostics.contains(expected),
                "expected {expected:?} in:\n{}",
                outcome.diagnostics
            );
        }
    }

    #[test]
    fn an_empty_input_is_not_an_error() {
        let outcome = process("type,client,tx,amount\n");
        assert_eq!(outcome.rejected, 0);
        assert_eq!(outcome.engine.accounts().count(), 0);
    }

    #[test]
    fn a_record_that_is_not_utf8_is_rejected_like_any_other() {
        let mut engine = Engine::new();
        let mut diagnostics = Vec::new();
        let input: &[u8] = b"type,client,tx,amount\n\
                             deposit,1,1,1.0\n\
                             deposit,1,\xff,1.0\n\
                             deposit,1,3,2.0\n";

        let rejected = run(input, &mut engine, &mut diagnostics).expect("not an I/O failure");

        assert_eq!(rejected, 1);
        assert_eq!(
            engine.account(1).expect("client 1").available(),
            dec("3.0"),
            "the records either side of the bad one still applied"
        );
    }

    /// Yields one valid chunk, then fails: the input stream is lost mid-run.
    struct FailsAfterFirstChunk {
        sent: bool,
    }

    impl std::io::Read for FailsAfterFirstChunk {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            const CHUNK: &[u8] = b"type,client,tx,amount\ndeposit,1,1,1.0\n";
            if self.sent || buffer.len() < CHUNK.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the stream went away",
                ));
            }
            self.sent = true;
            buffer[..CHUNK.len()].copy_from_slice(CHUNK);
            Ok(CHUNK.len())
        }
    }

    #[test]
    fn losing_the_input_stream_is_fatal() {
        let mut engine = Engine::new();
        let mut diagnostics = Vec::new();

        let error = run(
            FailsAfterFirstChunk { sent: false },
            &mut engine,
            &mut diagnostics,
        )
        .expect_err("a broken input stream must not be mistaken for a bad record");

        assert!(matches!(error, FatalError::Read(_)), "{error:?}");
    }
}
