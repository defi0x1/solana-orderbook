//! Decimal-aware conversions between raw atoms/ticks and UI units.
//! Integer math only: widen, multiply, divide with explicit rounding.

use core::fmt;

/// How an inexact division is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    Down,
    Up,
    Nearest,
}

/// Why a conversion could not be completed exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleError {
    /// Not a plain unsigned decimal (signed, exponent, empty or non-digit).
    Malformed,
    /// More fractional digits than the target can represent.
    TooPrecise,
    /// The exact result does not fit the target type.
    Overflow,
    /// A market parameter was zero where a divisor is required.
    DivideByZero,
}

impl fmt::Display for ScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Malformed => "not a plain unsigned decimal",
            Self::TooPrecise => "more fractional digits than representable",
            Self::Overflow => "result does not fit the target type",
            Self::DivideByZero => "market parameter is zero",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ScaleError {}

/// `x * y / denominator` widened to u128, resolved with `rounding`.
fn mul_div(x: u128, y: u128, denominator: u128, rounding: Rounding) -> Result<u128, ScaleError> {
    if denominator == 0 {
        return Err(ScaleError::DivideByZero);
    }
    let prod = x.checked_mul(y).ok_or(ScaleError::Overflow)?;
    let (quotient, remainder) = (prod / denominator, prod % denominator);
    let round_up = match rounding {
        Rounding::Down => false,
        Rounding::Up => remainder > 0,
        // Saturating is exact here: if 2*remainder overflows it exceeds any denominator.
        Rounding::Nearest => remainder.saturating_mul(2) >= denominator,
    };
    if round_up {
        quotient.checked_add(1).ok_or(ScaleError::Overflow)
    } else {
        Ok(quotient)
    }
}

fn pow10(exp: u32) -> Result<u128, ScaleError> {
    10u128.checked_pow(exp).ok_or(ScaleError::Overflow)
}

/// Render a scaled integer as a decimal string with `decimals` places.
fn render(value: u128, decimals: u8) -> Result<String, ScaleError> {
    let pow = pow10(decimals as u32)?;
    if decimals == 0 {
        return Ok(value.to_string());
    }
    let width = decimals as usize;
    Ok(format!("{}.{:0width$}", value / pow, value % pow, width = width))
}

/// Split a plain unsigned decimal into (mantissa, fractional digit count).
fn parse_decimal(s: &str) -> Result<(u128, u32), ScaleError> {
    let s = s.trim();
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(ScaleError::Malformed);
    }
    let mut mantissa: u128 = 0;
    for byte in int_part.bytes().chain(frac_part.bytes()) {
        let digit = (byte as char).to_digit(10).ok_or(ScaleError::Malformed)?;
        mantissa = mantissa
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit as u128))
            .ok_or(ScaleError::Overflow)?;
    }
    Ok((mantissa, frac_part.len() as u32))
}

/// Raw atoms as an exact decimal string: 1_500_000_000 at 9 decimals -> "1.500000000".
pub fn atoms_to_ui_amount(atoms: u64, decimals: u8) -> Result<String, ScaleError> {
    render(atoms as u128, decimals)
}

/// Inverse of [`atoms_to_ui_amount`]. Input finer than `decimals` is rejected, not truncated.
pub fn ui_amount_to_atoms(ui_amount: &str, decimals: u8) -> Result<u64, ScaleError> {
    let (mantissa, frac_digits) = parse_decimal(ui_amount)?;
    if frac_digits > decimals as u32 {
        return Err(ScaleError::TooPrecise);
    }
    let scale = pow10(decimals as u32 - frac_digits)?;
    let atoms = mantissa.checked_mul(scale).ok_or(ScaleError::Overflow)?;
    u64::try_from(atoms).map_err(|_| ScaleError::Overflow)
}

/// A tick's price as quote UI-units per base UI-unit, to `out_decimals` places.
/// price = tick * tick_size * 10^base_decimals / (base_lot_size * 10^quote_decimals)
pub fn tick_to_ui_price(
    tick: u32,
    tick_size: u64,
    base_lot_size: u64,
    base_decimals: u8,
    quote_decimals: u8,
    out_decimals: u8,
) -> Result<String, ScaleError> {
    let quote_atoms_per_lot = (tick as u128)
        .checked_mul(tick_size as u128)
        .ok_or(ScaleError::Overflow)?;
    let scale = pow10(base_decimals as u32)?
        .checked_mul(pow10(out_decimals as u32)?)
        .ok_or(ScaleError::Overflow)?;
    let denominator = (base_lot_size as u128)
        .checked_mul(pow10(quote_decimals as u32)?)
        .ok_or(ScaleError::Overflow)?;
    let scaled = mul_div(quote_atoms_per_lot, scale, denominator, Rounding::Nearest)?;
    render(scaled, out_decimals)
}

/// Inverse of [`tick_to_ui_price`]. Rounding is explicit so a quote never
/// crosses further than the caller intended.
pub fn ui_price_to_tick(
    ui_price: &str,
    tick_size: u64,
    base_lot_size: u64,
    base_decimals: u8,
    quote_decimals: u8,
    rounding: Rounding,
) -> Result<u32, ScaleError> {
    let (mantissa, frac_digits) = parse_decimal(ui_price)?;
    let numerator = (base_lot_size as u128)
        .checked_mul(pow10(quote_decimals as u32)?)
        .ok_or(ScaleError::Overflow)?;
    let denominator = (tick_size as u128)
        .checked_mul(pow10(base_decimals as u32)?)
        .ok_or(ScaleError::Overflow)?
        .checked_mul(pow10(frac_digits)?)
        .ok_or(ScaleError::Overflow)?;
    let tick = mul_div(mantissa, numerator, denominator, rounding)?;
    u32::try_from(tick).map_err(|_| ScaleError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every one of these is above f64's 53-bit mantissa and round-tripped lossily before.
    #[test]
    fn round_trips_atom_counts_f64_cannot_represent() {
        for atoms in [
            9_007_199_254_740_993u64,
            123_456_789_123_456_789,
            1_000_000_000_000_000_001,
            u64::MAX,
        ] {
            let ui = atoms_to_ui_amount(atoms, 9).expect("render");
            assert_eq!(ui_amount_to_atoms(&ui, 9), Ok(atoms), "lost precision on {atoms}");
        }
    }

    #[test]
    fn renders_exact_decimal_places() {
        assert_eq!(atoms_to_ui_amount(1_500_000_000, 9).unwrap(), "1.500000000");
        assert_eq!(atoms_to_ui_amount(1, 9).unwrap(), "0.000000001");
        assert_eq!(atoms_to_ui_amount(42, 0).unwrap(), "42");
    }

    /// The f64 version turned each of these into a plausible-looking number.
    #[test]
    fn rejects_input_f64_silently_accepted() {
        for bad in ["", ".", "-5", "1e30", "NaN", "inf", "1.2.3", "abc"] {
            assert_eq!(
                ui_amount_to_atoms(bad, 9),
                Err(ScaleError::Malformed),
                "accepted {bad:?}"
            );
        }
        assert_eq!(
            ui_amount_to_atoms("1.0000000001", 9),
            Err(ScaleError::TooPrecise)
        );
        assert_eq!(
            ui_amount_to_atoms("18446744073709551616", 0),
            Err(ScaleError::Overflow)
        );
    }

    #[test]
    fn price_round_trips_both_decimal_orientations() {
        for (tick_size, base_lot, base_dec, quote_dec, out_dec, tick) in [
            (100u64, 1_000u64, 9u8, 6u8, 6u8, 12_345u32),
            (50, 100, 6, 9, 9, 7_777),
        ] {
            let price =
                tick_to_ui_price(tick, tick_size, base_lot, base_dec, quote_dec, out_dec).unwrap();
            let back =
                ui_price_to_tick(&price, tick_size, base_lot, base_dec, quote_dec, Rounding::Nearest);
            assert_eq!(back, Ok(tick), "price {price} did not round-trip");
        }
    }

    /// A price landing exactly between two ticks resolves the way the caller asked.
    #[test]
    fn rounding_direction_is_the_callers_choice() {
        let half = |r| ui_price_to_tick("3", 2, 1, 0, 0, r);
        assert_eq!(half(Rounding::Down), Ok(1));
        assert_eq!(half(Rounding::Up), Ok(2));
        assert_eq!(half(Rounding::Nearest), Ok(2));
    }

    #[test]
    fn zero_market_parameter_is_an_error_not_a_panic() {
        assert_eq!(
            tick_to_ui_price(1, 1, 0, 0, 0, 0),
            Err(ScaleError::DivideByZero)
        );
        assert_eq!(
            ui_price_to_tick("1", 0, 1, 0, 0, Rounding::Nearest),
            Err(ScaleError::DivideByZero)
        );
    }
}
