//! Differential fuzz of `Bitmap` against a `BTreeSet` oracle.
//!
//! `tests/tests/bitmap.rs` already brute-forces the scans across every anchor
//! residue with hand-picked offsets and a fixed xorshift sweep; this target
//! covers the same public surface (`set`/`clear`/`get`/`next_at_or_after`/
//! `prev_at_or_before`) with libFuzzer-guided arbitrary operation sequences
//! instead of a fixed sweep, so it explores interactions the hand-picked
//! offsets don't happen to hit — particularly the two-segment physical/tick
//! wraparound split described on `Bitmap`'s doc comment, which is exactly
//! where a subtle off-by-one would hide.

#![no_main]

use arbitrary::Arbitrary;
use clob_program::test_utils::*;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

#[derive(Arbitrary, Debug)]
enum Op {
    Set { bid: bool, off: u16 },
    Clear { bid: bool, off: u16 },
    Get { bid: bool, off: u16 },
    Next { bid: bool, from_off: u16 },
    Prev { bid: bool, from_off: u16 },
}

#[derive(Arbitrary, Debug)]
struct Input {
    /// Anchor residue: which 64-tick word the window starts at.
    word: u8,
    /// Coarse anchor base, kept well under tick_limit.
    base_mult: u8,
    ops: Vec<Op>,
}

/// A bare market buffer, no orders — enough to exercise the bitmaps directly.
/// Mirrors `tests/tests/bitmap.rs::buffer`.
fn buffer(anchor: u32) -> Vec<u8> {
    let mut data = vec![0u8; POOL_OFFSET + MIN_POOL_CAPACITY as usize * NODE_LEN];
    let header = unsafe { Market::from_bytes_unchecked_mut(&mut data) };
    header.set_version(MARKET_VERSION);
    header.set_order_pool_capacity(MIN_POOL_CAPACITY);
    header.set_anchor_tick(anchor);
    header.set_tick_size(100);
    header.set_base_lot_size(1_000);
    header.set_max_lots_per_order(1_000_000);
    header.set_tick_limit(u32::MAX - 2 * WINDOW_TICKS as u32);
    data
}

fuzz_target!(|input: Input| {
    let word = (input.word as u32) % (WINDOW_TICKS / 64) as u32;
    // Multiple of 8192 keeps the anchor a multiple of 64 and comfortably
    // below the buffer's generous tick_limit regardless of word/base.
    let base = 8_192 * (1 + (input.base_mult as u32 % 64));
    let anchor = base + word * 64;
    let top = anchor + WINDOW_MASK;

    let mut data = buffer(anchor);
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };

    let mut bid_oracle: BTreeSet<u32> = BTreeSet::new();
    let mut ask_oracle: BTreeSet<u32> = BTreeSet::new();

    for op in input.ops.iter().take(512) {
        match *op {
            Op::Set { bid, off } => {
                let tick = anchor + (off as u32 % WINDOW_TICKS as u32);
                if bid {
                    book.bid_bitmap.set(tick);
                    bid_oracle.insert(tick);
                } else {
                    book.ask_bitmap.set(tick);
                    ask_oracle.insert(tick);
                }
            }
            Op::Clear { bid, off } => {
                let tick = anchor + (off as u32 % WINDOW_TICKS as u32);
                if bid {
                    book.bid_bitmap.clear(tick);
                    bid_oracle.remove(&tick);
                } else {
                    book.ask_bitmap.clear(tick);
                    ask_oracle.remove(&tick);
                }
            }
            Op::Get { bid, off } => {
                let tick = anchor + (off as u32 % WINDOW_TICKS as u32);
                let (got, want) = if bid {
                    (book.bid_bitmap.get(tick), bid_oracle.contains(&tick))
                } else {
                    (book.ask_bitmap.get(tick), ask_oracle.contains(&tick))
                };
                assert_eq!(
                    got, want,
                    "get() mismatch: tick {tick}, anchor {anchor}, bid {bid}"
                );
            }
            Op::Next { bid, from_off } => {
                let from = anchor + (from_off as u32 % WINDOW_TICKS as u32);
                let (got, oracle) = if bid {
                    (book.bid_bitmap.next_at_or_after(anchor, from), &bid_oracle)
                } else {
                    (book.ask_bitmap.next_at_or_after(anchor, from), &ask_oracle)
                };
                let want = oracle.range(from..=top).next().copied();
                assert_eq!(
                    got, want,
                    "next_at_or_after mismatch: from {from}, anchor {anchor}, bid {bid}"
                );
            }
            Op::Prev { bid, from_off } => {
                let from = anchor + (from_off as u32 % WINDOW_TICKS as u32);
                let (got, oracle) = if bid {
                    (book.bid_bitmap.prev_at_or_before(anchor, from), &bid_oracle)
                } else {
                    (book.ask_bitmap.prev_at_or_before(anchor, from), &ask_oracle)
                };
                let want = oracle.range(anchor..=from).next_back().copied();
                assert_eq!(
                    got, want,
                    "prev_at_or_before mismatch: from {from}, anchor {anchor}, bid {bid}"
                );
            }
        }
    }
});
