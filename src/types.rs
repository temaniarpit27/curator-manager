//! Basic types and checked arithmetic. Overflow is an error, never a wrapped
//! value or a panic.

use std::fmt;

use ethnum::U256;
use serde::Deserialize;
use thiserror::Error;

pub type BlockNumber = u64;

/// A string ID as its own type, so a strategy can't be passed as a vault.
/// `Borrow<str>` lets maps keyed by it be looked up with a `&str`.
macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        // `pad`, so widths like `{:<14}` apply.
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0)
            }
        }
    };
}

string_id!(VaultId);
string_id!(StrategyId);
string_id!(UserId);

/// An event's on-chain identity; its ordering is chain order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId {
    pub block: BlockNumber,
    pub log_index: u64,
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.block, self.log_index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MathError {
    #[error("overflow: {a} {op} {b}")]
    Overflow { a: u128, op: char, b: u128 },
    #[error("insufficient: have {have}, need {need}")]
    Insufficient { have: u128, need: u128 },
    #[error("division by zero")]
    DivByZero,
}

/// A `u128` amount with checked arithmetic only (no `Add`/`Sub`, which would
/// have to wrap or panic).
macro_rules! amount {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
        #[serde(transparent)]
        pub struct $name(u128);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(raw: u128) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u128 {
                self.0
            }

            pub fn checked_add(self, rhs: Self) -> Result<Self, MathError> {
                self.0.checked_add(rhs.0).map(Self).ok_or(MathError::Overflow {
                    a: self.0,
                    op: '+',
                    b: rhs.0,
                })
            }

            pub fn checked_sub(self, rhs: Self) -> Result<Self, MathError> {
                self.0
                    .checked_sub(rhs.0)
                    .map(Self)
                    .ok_or(MathError::Insufficient { have: self.0, need: rhs.0 })
            }

            pub fn checked_sum(items: impl IntoIterator<Item = Self>) -> Result<Self, MathError> {
                items.into_iter().try_fold(Self::ZERO, Self::checked_add)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

amount!(
    /// The vault's asset, in its smallest unit.
    Assets
);

amount!(
    /// Vault shares, in raw units (the feed gives no share decimals).
    Shares
);

/// Basis points in `0..=10_000`, checked when built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bps(u16);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0} bps is outside 0..=10000")]
pub struct BpsOutOfRange(pub u64);

impl TryFrom<u64> for Bps {
    type Error = BpsOutOfRange;

    fn try_from(raw: u64) -> Result<Self, BpsOutOfRange> {
        match u16::try_from(raw) {
            Ok(v) if v <= Self::MAX => Ok(Self(v)),
            _ => Err(BpsOutOfRange(raw)),
        }
    }
}

impl Bps {
    pub const MAX: u16 = 10_000;

    pub fn raw(self) -> u16 {
        self.0
    }

    /// `floor(total * self / 10_000)`. Never exceeds `total`.
    pub fn of(self, total: Assets) -> Result<Assets, MathError> {
        mul_div_floor(total.0, u128::from(self.0), u128::from(Self::MAX)).map(Assets)
    }
}

impl fmt::Display for Bps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}bps", self.0)
    }
}

/// `floor(a * b / c)`, multiplying in 256 bits so the product can't overflow.
pub fn mul_div_floor(a: u128, b: u128, c: u128) -> Result<u128, MathError> {
    if c == 0 {
        return Err(MathError::DivByZero);
    }
    let q = U256::from(a) * U256::from(b) / U256::from(c);
    u128::try_from(q).map_err(|_| MathError::Overflow { a, op: '*', b })
}

/// Render a raw amount with `decimals` places, e.g. `1500000` at 6 → `1.500000`.
pub fn format_units(raw: u128, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let decimals = usize::from(decimals);
    let padded = format!("{raw:0>width$}", width = decimals + 1);
    match padded.split_at_checked(padded.len().saturating_sub(decimals)) {
        Some((int, frac)) => format!("{int}.{frac}"),
        None => padded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_math_reports_errors() {
        assert!(matches!(
            Assets(u128::MAX).checked_add(Assets(1)),
            Err(MathError::Overflow { .. })
        ));
        assert_eq!(
            Shares(1).checked_sub(Shares(2)),
            Err(MathError::Insufficient { have: 1, need: 2 })
        );
        assert_eq!(Assets(5).checked_sub(Assets(3)), Ok(Assets(2)));
    }

    #[test]
    fn mul_div_floor_uses_wide_intermediate() {
        assert_eq!(mul_div_floor(10, 1, 3), Ok(3));
        assert_eq!(mul_div_floor(1, 1, 0), Err(MathError::DivByZero));
        // The product overflows u128 but the result fits: must succeed.
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, u128::MAX), Ok(u128::MAX));
        // The result itself does not fit: must be an error.
        assert!(matches!(mul_div_floor(u128::MAX, 2, 1), Err(MathError::Overflow { .. })));
    }

    #[test]
    fn bps_is_bounded_and_floors() {
        assert!(Bps::try_from(10_000).is_ok());
        assert_eq!(Bps::try_from(10_001), Err(BpsOutOfRange(10_001)));
        assert_eq!(Bps::try_from(70_000), Err(BpsOutOfRange(70_000)));
        let sixty = Bps::try_from(6_000).unwrap();
        assert_eq!(sixty.of(Assets(184_000_000_000)), Ok(Assets(110_400_000_000)));
        assert_eq!(Bps::try_from(3_333).unwrap().of(Assets(10)), Ok(Assets(3)));
    }

    #[test]
    fn format_units_pads_small_values() {
        assert_eq!(format_units(1_500_000, 6), "1.500000");
        assert_eq!(format_units(5, 6), "0.000005");
        assert_eq!(format_units(42, 0), "42");
    }
}
