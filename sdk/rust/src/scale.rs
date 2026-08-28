//! Decimal-aware conversions between raw atoms/ticks and UI-facing units.
//!
//! The on-chain program only ever deals in raw atoms, lots and ticks — it
//! never needs a mint's decimals for anything but `TransferChecked`. An
//! integrator displaying or accepting human-readable amounts and prices does
//! need that conversion, and nothing in this SDK previously did it.

/// Raw atoms to a human-readable amount, e.g. `1_500_000_000` at 9 decimals -> `1.5`.
pub fn atoms_to_ui_amount(atoms: u64, decimals: u8) -> f64 {
    atoms as f64 / 10f64.powi(decimals as i32)
}

/// Inverse of [`atoms_to_ui_amount`], rounded to the nearest atom.
pub fn ui_amount_to_atoms(ui_amount: f64, decimals: u8) -> u64 {
    (ui_amount * 10f64.powi(decimals as i32)).round() as u64
}

/// A tick's price (`tick * tick_size` quote atoms per base lot) as quote
/// UI-units per base UI-unit, accounting for both mints' decimals.
pub fn tick_to_ui_price(
    tick: u32,
    tick_size: u64,
    base_lot_size: u64,
    base_decimals: u8,
    quote_decimals: u8,
) -> f64 {
    let quote_atoms_per_base_atom = (tick as f64 * tick_size as f64) / base_lot_size as f64;
    quote_atoms_per_base_atom * 10f64.powi(base_decimals as i32 - quote_decimals as i32)
}

/// Inverse of [`tick_to_ui_price`], rounded to the nearest tick.
pub fn ui_price_to_tick(
    ui_price: f64,
    tick_size: u64,
    base_lot_size: u64,
    base_decimals: u8,
    quote_decimals: u8,
) -> u32 {
    let quote_atoms_per_base_atom = ui_price / 10f64.powi(base_decimals as i32 - quote_decimals as i32);
    ((quote_atoms_per_base_atom * base_lot_size as f64) / tick_size as f64).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoms_ui_amount_round_trips() {
        assert_eq!(atoms_to_ui_amount(1_500_000_000, 9), 1.5);
        assert_eq!(ui_amount_to_atoms(1.5, 9), 1_500_000_000);
    }

    #[test]
    fn tick_ui_price_round_trips_sol_usdc_like_pair() {
        let (tick_size, base_lot_size, base_decimals, quote_decimals) = (100u64, 1_000u64, 9u8, 6u8);
        let ui_price = tick_to_ui_price(12_345, tick_size, base_lot_size, base_decimals, quote_decimals);
        let tick = ui_price_to_tick(ui_price, tick_size, base_lot_size, base_decimals, quote_decimals);
        assert_eq!(tick, 12_345);
    }

    #[test]
    fn tick_ui_price_round_trips_when_base_has_fewer_decimals() {
        let (tick_size, base_lot_size, base_decimals, quote_decimals) = (50u64, 100u64, 6u8, 9u8);
        let ui_price = tick_to_ui_price(7_777, tick_size, base_lot_size, base_decimals, quote_decimals);
        let tick = ui_price_to_tick(ui_price, tick_size, base_lot_size, base_decimals, quote_decimals);
        assert_eq!(tick, 7_777);
    }
}
