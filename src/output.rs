//! Rendering the final account states as CSV.

use std::io::Write;

use rust_decimal::Decimal;

use crate::{account::AccountRow, amount::SCALE, error::FatalError};

/// Writes account rows as they arrive.
///
/// Rows are written the moment they are handed over, which is what lets a frozen account
/// leave the engine mid-run instead of being carried to the end. The consequence is that
/// output order follows the order accounts finish in, not client order: with shards
/// running concurrently that order is not reproducible between runs. The specification
/// states row order is irrelevant, and the tests compare row sets rather than bytes.
pub struct RowWriter<W: Write> {
    csv: csv::Writer<W>,
}

impl<W: Write> RowWriter<W> {
    /// Opens the output and writes the header.
    pub fn new(writer: W) -> Result<Self, FatalError> {
        let mut csv = csv::Writer::from_writer(writer);
        csv.write_record(["client", "available", "held", "total", "locked"])
            .map_err(FatalError::Write)?;
        Ok(Self { csv })
    }

    /// Writes one row.
    pub fn write(&mut self, row: AccountRow) -> Result<(), FatalError> {
        self.csv
            .write_record([
                row.client.to_string(),
                render(row.available),
                render(row.held),
                render(row.total),
                row.locked.to_string(),
            ])
            .map_err(FatalError::Write)
    }

    /// Flushes everything buffered.
    pub fn finish(mut self) -> Result<(), FatalError> {
        self.csv
            .flush()
            .map_err(|error| FatalError::Write(csv::Error::from(error)))
    }
}

/// Writes a finished set of rows, in ascending client order.
///
/// Used by the single-threaded path, where every row is in hand at the end anyway and
/// sorting 65,536 of them costs nothing.
pub fn write<W: Write>(rows: Vec<AccountRow>, writer: W) -> Result<(), FatalError> {
    let mut rows = rows;
    rows.sort_unstable_by_key(|row| row.client);

    let mut out = RowWriter::new(writer)?;
    for row in rows {
        out.write(row)?;
    }
    out.finish()
}

/// Formats a balance with exactly [`SCALE`] decimal places.
///
/// Deliberately not `format!("{value:.4}")`: asking `rust_decimal` for an explicit
/// precision panics once the value has enough integer digits, and an input file can push
/// a balance that high. Padding the plain `Display` output cannot fail.
fn render(value: Decimal) -> String {
    let text = value.to_string();
    let scale = SCALE as usize;

    let (integer, fraction) = text.split_once('.').unwrap_or((text.as_str(), ""));
    // `saturating_sub` rather than the arithmetic: balances are sums of values validated
    // to at most `SCALE` places, so a longer fraction is unreachable, but underflowing
    // here would ask for a `repeat` of about 18 quintillion zeroes.
    let padding = "0".repeat(scale.saturating_sub(fraction.len()));
    format!("{integer}.{fraction}{padding}")
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{amount::Amount, engine::Engine, record::Transaction};

    fn dec(raw: &str) -> Decimal {
        Decimal::from_str(raw).expect("valid decimal")
    }

    fn rendered(engine: &mut Engine) -> String {
        let mut buffer = Vec::new();
        write(engine.drain_rows(), &mut buffer).expect("writing to a Vec cannot fail");
        String::from_utf8(buffer).expect("output is ASCII")
    }

    #[test]
    fn pads_to_four_decimal_places() {
        assert_eq!(render(dec("0")), "0.0000");
        assert_eq!(render(dec("1.5")), "1.5000");
        assert_eq!(render(dec("1.0001")), "1.0001");
        assert_eq!(render(dec("-1.5")), "-1.5000");
    }

    #[test]
    fn renders_extreme_values_without_panicking() {
        assert_eq!(render(Decimal::MAX), "79228162514264337593543950335.0000");
        assert_eq!(render(Decimal::MIN), "-79228162514264337593543950335.0000");
    }

    #[test]
    fn writes_a_header_even_with_no_accounts() {
        assert_eq!(
            rendered(&mut Engine::new()),
            "client,available,held,total,locked\n"
        );
    }

    #[test]
    fn rows_are_ordered_by_client() {
        let mut engine = Engine::new();
        for client in [9, 1, 5] {
            engine
                .apply(Transaction::Deposit {
                    client,
                    tx: u32::from(client),
                    amount: Amount::parse("1.0").expect("valid amount"),
                })
                .expect("deposit");
        }

        assert_eq!(
            rendered(&mut engine),
            "client,available,held,total,locked\n\
             1,1.0000,0.0000,1.0000,false\n\
             5,1.0000,0.0000,1.0000,false\n\
             9,1.0000,0.0000,1.0000,false\n"
        );
    }
}
