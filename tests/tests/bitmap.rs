//! Differential test of the bitmap scans against a naive oracle, across every
//! anchor residue.
//!
//! The scans are the most subtle code in the program: they must return results
//! in TICK order over a window that wraps the physical array, which means up to
//! two segments and index arithmetic in three different units (absolute tick,
//! physical position, leaf word, summary group). This test brute-forces them
//! rather than trusting the reasoning.

use clob_program::test_utils::*;

/// A market buffer with a chosen anchor and no orders — enough to exercise the
/// bitmaps directly.
fn buffer(anchor: u32) -> Vec<u8> {
    let mut data = vec![0u8; POOL_OFFSET + 64 * NODE_LEN];
    {
        let header = unsafe { Market::from_bytes_unchecked_mut(&mut data) };
        header.set_version(MARKET_VERSION);
        header.set_order_pool_capacity(64);
        header.set_anchor_tick(anchor);
        header.set_tick_size(100);
        header.set_base_lot_size(1_000);
        header.set_max_lots_per_order(1_000_000);
        header.set_tick_limit(u32::MAX - 2 * WINDOW_TICKS as u32);
    }
    data
}

/// Naive oracle: walk the window in tick order and report the highest/lowest
/// occupied tick.
fn oracle_highest(book: &BookRefMut, anchor: u32, bid: bool) -> Option<u32> {
    (0..WINDOW_TICKS as u32).rev().find_map(|off| {
        let t = anchor + off;
        let set = if bid {
            book.bid_bitmap.get(t)
        } else {
            book.ask_bitmap.get(t)
        };
        if set {
            Some(t)
        } else {
            None
        }
    })
}

fn oracle_lowest(book: &BookRefMut, anchor: u32, bid: bool) -> Option<u32> {
    (0..WINDOW_TICKS as u32).find_map(|off| {
        let t = anchor + off;
        let set = if bid {
            book.bid_bitmap.get(t)
        } else {
            book.ask_bitmap.get(t)
        };
        if set {
            Some(t)
        } else {
            None
        }
    })
}

/// Single set bit, swept across every anchor residue and a spread of offsets.
/// `best_bid` must equal the oracle for every combination.
#[test]
fn best_bid_matches_oracle_for_every_anchor_residue() {
    let mut failures: Vec<(u32, u32)> = Vec::new();
    let mut checked = 0u32;

    // Every distinct anchor residue class (anchors are multiples of 64).
    for word in 0..(WINDOW_TICKS / 64) as u32 {
        let anchor = 8_192 * 8 + word * 64; // arbitrary base, residue = word*64
        for off in [
            0u32, 1, 63, 64, 65, 127, 128, 1_000, 2_047, 2_048, 4_095, 4_096, 4_097, 6_000, 8_190,
            8_191,
        ] {
            let mut data = buffer(anchor);
            let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
            let tick = anchor + off;
            book.bid_bitmap.set(tick);

            let got = book.best_bid();
            let want = oracle_highest(&book, anchor, true);
            checked += 1;
            if got != want {
                failures.push((anchor, tick));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "best_bid() disagreed with the oracle on {} of {checked} single-bit \
         cases. First 8: {:?}. A None here means the program cannot see a \
         resting bid that is really there.",
        failures.len(),
        &failures[..failures.len().min(8)]
    );
}

/// The mirror, which the auditor expects to be correct — kept so a future fix
/// to `scan_down` cannot silently break `scan_up`.
#[test]
fn best_ask_matches_oracle_for_every_anchor_residue() {
    let mut failures: Vec<(u32, u32)> = Vec::new();

    for word in 0..(WINDOW_TICKS / 64) as u32 {
        let anchor = 8_192 * 8 + word * 64;
        for off in [
            0u32, 1, 63, 64, 65, 127, 128, 1_000, 2_047, 2_048, 4_095, 4_096, 4_097, 6_000, 8_190,
            8_191,
        ] {
            let mut data = buffer(anchor);
            let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
            let tick = anchor + off;
            book.ask_bitmap.set(tick);

            if book.best_ask() != oracle_lowest(&book, anchor, false) {
                failures.push((anchor, tick));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "best_ask() disagreed with the oracle on {} cases. First 8: {:?}",
        failures.len(),
        &failures[..failures.len().min(8)]
    );
}

/// Multi-bit sweep: many bits set at once, every anchor residue, both scan
/// directions, checked against the oracle. Single-bit tests can miss
/// interactions between the summary word and the leaf words.
#[test]
fn scans_match_oracle_with_many_bits_set() {
    // Deterministic xorshift — no rand dependency, reproducible failures.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let mut checked = 0u32;
    let mut failures = Vec::new();

    for word in 0..(WINDOW_TICKS / 64) as u32 {
        let anchor = 8_192 * 8 + word * 64;
        for round in 0..6 {
            let mut data = buffer(anchor);
            let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };

            // Sprinkle 1..=24 bids and asks on disjoint ticks.
            let count = 1 + (next() % 24) as u32;
            let mut used = std::collections::BTreeSet::new();
            for _ in 0..count {
                let off = (next() % WINDOW_TICKS as u64) as u32;
                if !used.insert(off) {
                    continue;
                }
                let t = anchor + off;
                if next() % 2 == 0 {
                    book.bid_bitmap.set(t);
                } else {
                    book.ask_bitmap.set(t);
                }
            }

            checked += 1;
            if book.best_bid() != oracle_highest(&book, anchor, true) {
                failures.push((anchor, round, "best_bid"));
            }
            if book.best_ask() != oracle_lowest(&book, anchor, false) {
                failures.push((anchor, round, "best_ask"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {checked} multi-bit cases disagreed with the oracle. First 8: {:?}",
        failures.len(),
        &failures[..failures.len().min(8)]
    );
}

/// The exact reproduction from the audit, isolated so the failure message is
/// unambiguous.
#[test]
fn best_bid_sees_a_bid_that_is_really_there() {
    let anchor = 59_968u32;
    let tick = 60_032u32;

    let mut data = buffer(anchor);
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    book.bid_bitmap.set(tick);

    assert!(book.bid_bitmap.get(tick), "precondition: the bit is set");
    assert_eq!(
        book.best_bid(),
        Some(tick),
        "best_bid() is blind to a resting bid at tick {tick} with anchor \
         {anchor} (residue {}). An ask can then be posted on the same tick, \
         putting both bitmaps on one level.",
        anchor % WINDOW_TICKS as u32
    );
}
