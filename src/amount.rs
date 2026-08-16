//! Validated monetary amounts.

use std::fmt;

use rust_decimal::Decimal;

use crate::error::RecordError;

/// The number of decimal places the engine works in.
pub const SCALE: u32 = 4;

/// A monetary amount that has been checked at the input boundary: a valid decimal, not
/// negative, and no finer than [`SCALE`] decimal places.
///
/// Constructing one is the only way into the engine, so no code past the boundary has to
/// wonder whether a value is well-formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(Decimal);

impl Amount {
    /// Parses and validates an amount as written in the input.
    pub fn parse(raw: &str) -> Result<Self, RecordError> {
        // `from_str_exact` refuses to round, but it happily accepts more precision than
        // we allow, so the scale is checked separately rather than silently rescaled.
        let value = Decimal::from_str_exact(raw)
            .map_err(|_| RecordError::MalformedAmount(raw.to_owned()))?;

        if value.is_sign_negative() {
            return Err(RecordError::NegativeAmount(raw.to_owned()));
        }
        if value.scale() > SCALE {
            return Err(RecordError::ExcessPrecision(raw.to_owned()));
        }

        Ok(Self(value))
    }

    /// The underlying decimal.
    pub fn value(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately not `{:.4}`: `rust_decimal` panics when asked for an explicit
        // precision on a value with many integer digits. See `crate::output::render`.
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Decimal, RecordError> {
        Amount::parse(raw).map(Amount::value)
    }

    #[test]
    fn accepts_up_to_four_decimals() {
        assert_eq!(parse("1.0001").unwrap().to_string(), "1.0001");
        assert_eq!(parse("1.0").unwrap().to_string(), "1.0");
        assert_eq!(parse("0").unwrap().to_string(), "0");
    }

    #[test]
    fn accepts_zero() {
        assert_eq!(parse("0.0000").unwrap(), Decimal::ZERO);
        assert_eq!(parse("-0.0").unwrap(), Decimal::ZERO);
    }

    #[test]
    fn rejects_excess_precision_rather_than_rounding() {
        assert!(matches!(
            parse("1.00001"),
            Err(RecordError::ExcessPrecision(_))
        ));
    }

    #[test]
    fn rejects_negative() {
        assert!(matches!(parse("-1.0"), Err(RecordError::NegativeAmount(_))));
    }

    #[test]
    fn rejects_garbage() {
        for raw in ["", "abc", "1.2.3", "1,0", "--1"] {
            assert!(
                matches!(parse(raw), Err(RecordError::MalformedAmount(_))),
                "{raw} should not parse"
            );
        }
    }

    #[test]
    fn accepts_the_largest_representable_value() {
        assert!(parse("79228162514264337593543950335").is_ok());
        assert!(matches!(
            parse("79228162514264337593543950336"),
            Err(RecordError::MalformedAmount(_))
        ));
    }
}
