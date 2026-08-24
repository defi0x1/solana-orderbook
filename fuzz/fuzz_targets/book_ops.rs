//! Stateful fuzz of `BookRefMut`'s order lifecycle: a sequence of
//! post/cancel/take/mass_quote/reanchor/reap operations against one
//! freshly-initialized in-memory market, checking the book's own structural
//! invariants after every single operation.
//!
//! This drives exactly the code the project's `state.rs` doc comment singles
//! out as deliberately decoupled from `AccountInfo`/CPI so it can run
//! off-chain: the ring-buffer window, the doubly-linked node lifecycle, and
//! the mass-quote merge. Unit tests cover the cases someone thought to write;
//! this explores operation *sequences* nobody thought to write, which is
//! where use-after-free-style linked-list bugs and ring-wraparound off-by-ones
//! actually hide.
//!
//! No `AccountInfo`, no vaults, no CPI: seats are funded directly (the
//! logic-layer equivalent of a `Deposit`) so the fuzzer spends its budget on
//! order-lifecycle transitions instead of on funding plumbing. `claim_seat`
//! itself is a simple linear scan+set and is exercised once at setup rather
//! than fuzzed per-op, to keep the operation stream focused on the order
//! lifecycle, which is the actual target here.
//!
//! `take()` is documented as deliberately NOT settling the taker leg — its
//! callers (`PlaceTake`, `PlaceLimit`, `Swap`) do that immediately afterward
//! in the same instruction, and Solana reverts the whole instruction
//! (including `take()`'s maker-side mutations) if that settlement fails. The
//! `Take` arm below reproduces both halves of that contract: it settles the
//! taker leg itself (against a tracked seat, or against the virtual vault for
//! the seatless/`NO_SEAT` case, mirroring `Swap`'s CPIs) and snapshots/
//! restores the whole buffer to reproduce instruction-level atomicity when
//! that settlement — or `take()` itself — fails. No other op needs this: the
//! rest of `BookRefMut`'s methods are individually self-consistent even when
//! they return an error partway through (each mutating sub-step is a no-op on
//! failure or a locked<->free transfer within one seat), so a fuzzer-observed
//! partial application never itself violates a structural or conservation
//! invariant.

#![no_main]

use arbitrary::Arbitrary;
use clob_program::test_utils::*;
use libfuzzer_sys::fuzz_target;

const N_SEATS: u16 = 4;
const CAPACITY: u32 = 256;
const TICK_SIZE: u64 = 100;
const BASE_LOT_SIZE: u64 = 1_000;
const MAX_LOTS: u32 = 1_000_000;
const ANCHOR0: u32 = 64_000;
const SEAT_BASE_FUNDING: u64 = 1_000_000_000;
const SEAT_QUOTE_FUNDING: u64 = 1_000_000_000_000;

#[derive(Arbitrary, Debug)]
enum Op {
    Place {
        seat: u8,
        bid: bool,
        tick_off: u16,
        lots: u32,
        expiry_delta: u16,
        slide: bool,
    },
    CancelKth {
        seat: u8,
        k: u8,
    },
    Take {
        bid: bool,
        tick_off: u16,
        max_lots: u32,
        bounded_quote: bool,
        quote_budget: u64,
        taker_seat: u8,
        seatless: bool,
        abort_self_trade: bool,
    },
    MassQuote {
        seat: u8,
        bids: Vec<(u16, u32, u8)>,
        asks: Vec<(u16, u32, u8)>,
        expiry_delta: u16,
    },
    Reanchor {
        target_off: i16,
        max_levels: u32,
    },
    ReapExpired {
        bid: bool,
        start_off: u16,
        max_count: u32,
    },
    AdvanceSlot {
        delta: u16,
    },
}

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

/// Fresh market buffer, `N_SEATS` claimed and funded directly (the pure-logic
/// equivalent of a `Deposit`, with no CPI). Returns the buffer and the
/// virtual vault totals it is meant to conserve against.
fn setup() -> (Vec<u8>, u64, u64) {
    let mut data = vec![0u8; POOL_OFFSET + CAPACITY as usize * NODE_LEN];
    {
        let header = unsafe { Market::from_bytes_unchecked_mut(&mut data) };
        header.set_version(MARKET_VERSION);
        header.set_order_pool_capacity(CAPACITY);
        header.set_anchor_tick(ANCHOR0);
        header.set_tick_size(TICK_SIZE);
        header.set_base_lot_size(BASE_LOT_SIZE);
        header.set_max_lots_per_order(MAX_LOTS);
        header.set_tick_limit(u32::MAX - 2 * WINDOW_TICKS as u32);
        header.set_free_head(NIL);
    }
    let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    book.thread_free_list(1, CAPACITY);

    for i in 0..N_SEATS {
        let owner = [i as u8 + 1; 32];
        let idx = book.claim_seat(&owner).expect("seat claim");
        assert_eq!(idx, i, "seats claimed out of order");
        let seat = &mut book.seats[idx as usize];
        seat.set_base_free(SEAT_BASE_FUNDING);
        seat.set_quote_free(SEAT_QUOTE_FUNDING);
    }

    (data, SEAT_BASE_FUNDING * N_SEATS as u64, SEAT_QUOTE_FUNDING * N_SEATS as u64)
}

/// Structural invariants that must hold after every single operation.
/// Mirrors `tests/src/lib.rs::check_invariants` (I1-I9), with the vault
/// conservation check (I1) against the harness's tracked virtual vault
/// totals instead of real token accounts, plus cycle-safe list walks (a
/// linked-list bug here would otherwise hang the fuzz process instead of
/// reporting a crash).
fn check_invariants(data: &mut [u8], virtual_base_vault: u64, virtual_quote_vault: u64) {
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(data) };
    let anchor = book.header.anchor_tick();
    let capacity = book.header.order_pool_capacity() as usize;

    if let (Some(bb), Some(ba)) = (book.best_bid(), book.best_ask()) {
        assert!(bb < ba, "crossed book: best_bid {bb} >= best_ask {ba}");
    }

    // 0 unseen, 1 live-in-level, 2 free, 4 live-in-level-and-chain
    let mut node_state = vec![0u8; capacity];

    for offset in 0..WINDOW_TICKS as u32 {
        let tick = anchor + offset;
        let bid = book.bid_bitmap.get(tick);
        let ask = book.ask_bitmap.get(tick);
        assert!(!(bid && ask), "tick {tick} set on both sides");
        let lvl = &book.levels[(tick & WINDOW_MASK) as usize];
        if bid || ask {
            let mut cur = lvl.head();
            let mut sum = 0u64;
            let mut steps = 0u32;
            assert_ne!(cur, NIL, "occupied tick {tick} with empty level");
            while cur != NIL {
                steps += 1;
                assert!(steps <= capacity as u32, "level {tick} FIFO cycle");
                let node = &book.pool[cur as usize];
                assert!(node.lots() > 0, "zero-lot node {cur} linked in level {tick}");
                assert_ne!(node.seq(), FREED_SEQ, "freed node {cur} linked in level {tick}");
                assert_eq!(node.tick(), tick, "node {cur} tick mismatch");
                let expected_side = if bid { Side::Bid } else { Side::Ask };
                assert!(
                    node.side() == expected_side,
                    "node {cur} side disagrees with level bitmap at tick {tick}"
                );
                assert_eq!(node_state[cur as usize], 0, "node {cur} in two levels");
                node_state[cur as usize] = 1;
                sum += node.lots() as u64;
                cur = node.next();
            }
            assert_eq!(sum, lvl.total_lots(), "level {tick} total mismatch");
        } else {
            assert_eq!(lvl.head(), NIL, "unoccupied level {tick} not zeroed");
            assert_eq!(lvl.total_lots(), 0, "unoccupied level {tick} has lots");
        }
    }
    assert_eq!(node_state[0], 0, "reserved node 0 linked in a level");

    let mut free = 0u32;
    let mut cur = book.header.free_head();
    while cur != NIL {
        assert_eq!(node_state[cur as usize], 0, "free node {cur} also live");
        assert_eq!(book.pool[cur as usize].seq(), FREED_SEQ, "free node {cur} without FREED_SEQ");
        node_state[cur as usize] = 2;
        free += 1;
        assert!(free < capacity as u32, "free list cycle");
        cur = book.pool[cur as usize].next();
    }
    assert_eq!(free, book.header.free_count(), "free_count mismatch");

    let mut base_total = 0u64;
    let mut quote_total = 0u64;
    for (i, seat) in book.seats.iter().enumerate() {
        if seat.is_free() {
            continue;
        }
        base_total += seat.base_free() + seat.base_locked();
        quote_total += seat.quote_free() + seat.quote_locked();

        let mut base_locked = 0u64;
        let mut quote_locked = 0u64;
        let mut count = 0u16;
        let mut prev = NIL;
        let mut cur = seat.orders_head();
        let mut last_key = 0u64;
        let mut steps = 0u32;
        while cur != NIL {
            steps += 1;
            assert!(steps <= capacity as u32, "seat {i} chain cycle");
            let node = &book.pool[cur as usize];
            assert_eq!(node.maker() as usize, i, "node {cur} maker mismatch");
            assert_eq!(node.maker_prev(), prev, "node {cur} back-link broken");
            assert!(node.lots() > 0, "seat {i} chains node {cur} with no lots");
            assert_eq!(node_state[cur as usize], 1, "chained node {cur} not in a level");
            node_state[cur as usize] = 4;
            count += 1;
            let side = node.side();
            assert!(
                match side {
                    Side::Bid => book.bid_bitmap.get(node.tick()),
                    Side::Ask => book.ask_bitmap.get(node.tick()),
                },
                "node {cur} stored side disagrees with bitmap"
            );
            let key = ((side as u64) << 32) | node.tick() as u64;
            assert!(key > last_key, "seat {i} chain unsorted or tick duplicated");
            last_key = key;
            match side {
                Side::Bid => quote_locked += node.tick() as u64 * TICK_SIZE * node.lots() as u64,
                Side::Ask => base_locked += node.lots() as u64 * BASE_LOT_SIZE,
            }
            prev = cur;
            cur = node.maker_next();
        }
        assert_eq!(count, seat.order_count(), "seat {i} order_count mismatch");
        assert_eq!(base_locked, seat.base_locked(), "seat {i} base lock mismatch");
        assert_eq!(quote_locked, seat.quote_locked(), "seat {i} quote lock mismatch");
    }
    for (idx, state) in node_state.iter().enumerate() {
        assert_ne!(*state, 1, "live node {idx} not in any seat chain");
    }

    assert_eq!(base_total, virtual_base_vault, "base conservation violated");
    assert_eq!(
        quote_total + book.header.fees_accrued_quote(),
        virtual_quote_vault,
        "quote conservation violated"
    );
}

/// Encode a mass-quote payload: sorted-ascending, deduped, lots clamped into
/// range, capped to a small count so it fits the harness's small pool.
fn encode_quotes(raw: &[(u16, u32, u8)], max_lots: u32) -> (Vec<u8>, usize) {
    let mut ticks: Vec<(u32, u32, u8)> = Vec::new();
    for &(tick_off, lots, flags) in raw.iter().take(8) {
        let tick = ANCHOR0 + (tick_off as u32 % WINDOW_TICKS as u32);
        if tick == 0 {
            continue;
        }
        let lots = 1 + (lots % max_lots);
        if let Some(existing) = ticks.iter_mut().find(|(t, ..)| *t == tick) {
            *existing = (tick, lots, flags);
        } else {
            ticks.push((tick, lots, flags));
        }
    }
    ticks.sort_by_key(|(t, ..)| *t);
    let count = ticks.len();
    let mut out = Vec::with_capacity(count * 9);
    for (tick, lots, flags) in ticks {
        out.extend_from_slice(&tick.to_le_bytes());
        out.extend_from_slice(&lots.to_le_bytes());
        out.push(flags);
    }
    (out, count)
}

fuzz_target!(|input: Input| {
    let (mut data, mut virtual_base_vault, mut virtual_quote_vault) = setup();
    let mut slot: u64 = 1_000;

    for op in input.ops.iter().take(400) {
        match op {
            Op::Place {
                seat,
                bid,
                tick_off,
                lots,
                expiry_delta,
                slide,
            } => {
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let seat = *seat as u16 % N_SEATS;
                let anchor = book.header.anchor_tick();
                let tick = anchor.wrapping_add(*tick_off as u32 % (2 * WINDOW_TICKS as u32));
                let side = if *bid { Side::Bid } else { Side::Ask };
                let expiry = if *expiry_delta == 0 {
                    0
                } else {
                    (slot + *expiry_delta as u64) as u32
                };
                let _ = book.upsert_order(side, tick, *lots % (MAX_LOTS + 1), seat, expiry, *slide);
            }
            Op::CancelKth { seat, k } => {
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let seat = *seat as u16 % N_SEATS;
                let head = book.seats[seat as usize].orders_head();
                if head != NIL {
                    let count = book.seats[seat as usize].order_count();
                    let target = *k as u16 % count;
                    let mut cur = head;
                    for _ in 0..target {
                        cur = book.pool[cur as usize].maker_next();
                    }
                    let seq = book.pool[cur as usize].seq();
                    book.cancel_order(seat, cur, seq)
                        .expect("cancel of a live handle we just walked to");
                }
            }
            Op::Take {
                bid,
                tick_off,
                max_lots,
                bounded_quote,
                quote_budget,
                taker_seat,
                seatless,
                abort_self_trade,
            } => {
                // take() only settles the maker leg; its callers settle the
                // taker leg and, on failure, the whole instruction (including
                // take()'s effects) reverts. Reproduce both halves here.
                let snapshot = data.clone();
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let anchor = book.header.anchor_tick();
                let limit_tick = anchor.wrapping_add(*tick_off as u32 % (2 * WINDOW_TICKS as u32));
                let side = if *bid { Side::Bid } else { Side::Ask };
                let max_quote = if *bounded_quote { *quote_budget } else { u64::MAX };
                let taker_seat_idx = if *seatless {
                    NO_SEAT
                } else {
                    *taker_seat as u16 % N_SEATS
                };
                let policy = if *abort_self_trade {
                    SelfTradePolicy::Abort
                } else {
                    SelfTradePolicy::CancelResting
                };
                let result = book.take_for_test(
                    side,
                    limit_tick,
                    *max_lots % (MAX_LOTS + 1),
                    max_quote,
                    taker_seat_idx,
                    slot,
                    policy,
                );
                match result {
                    Err(e) => {
                        assert!(!is_invalid_state(&e), "take() reported book corruption: {op:?}");
                        data = snapshot; // whole instruction reverts, take()'s partial fills included
                    }
                    Ok(res) if taker_seat_idx == NO_SEAT => {
                        // Swap-equivalent: settled via vault CPIs against an
                        // untracked external party. The maker leg inside
                        // take() already balances against those CPIs by
                        // construction (see module doc), so this can never
                        // need to revert — just mirror the vault movement.
                        let base_atoms = book.header.base_atoms(res.lots);
                        match side {
                            Side::Bid => {
                                virtual_quote_vault += res.quote + res.fee;
                                virtual_base_vault -= base_atoms;
                            }
                            Side::Ask => {
                                virtual_base_vault += base_atoms;
                                virtual_quote_vault -= res.quote - res.fee;
                            }
                        }
                    }
                    Ok(res) => {
                        let base_atoms = book.header.base_atoms(res.lots);
                        let s = &mut book.seats[taker_seat_idx as usize];
                        let settled = match side {
                            Side::Bid => {
                                let cost = res.quote + res.fee;
                                if s.quote_free() < cost {
                                    false
                                } else {
                                    s.set_quote_free(s.quote_free() - cost);
                                    s.set_base_free(s.base_free().wrapping_add(base_atoms));
                                    true
                                }
                            }
                            Side::Ask => {
                                if s.base_free() < base_atoms {
                                    false
                                } else {
                                    s.set_base_free(s.base_free() - base_atoms);
                                    s.set_quote_free(s.quote_free().wrapping_add(res.quote - res.fee));
                                    true
                                }
                            }
                        };
                        if !settled {
                            data = snapshot; // InsufficientBalance: revert the whole instruction
                        }
                    }
                }
            }
            Op::MassQuote {
                seat,
                bids,
                asks,
                expiry_delta,
            } => {
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let seat = *seat as u16 % N_SEATS;
                let (bid_bytes, bid_count) = encode_quotes(bids, MAX_LOTS);
                let (ask_bytes, ask_count) = encode_quotes(asks, MAX_LOTS);
                let expiry = if *expiry_delta == 0 {
                    0
                } else {
                    (slot + *expiry_delta as u64) as u32
                };
                let head = book.seats[seat as usize].orders_head();
                let cursor =
                    book.mass_quote_side(seat, Side::Bid, &bid_bytes, bid_count, expiry, (NIL, head));
                let cursor = match cursor {
                    Ok(c) => c,
                    Err(e) => {
                        assert!(!is_invalid_state(&e), "mass_quote_side(bid) reported corruption: {op:?}");
                        continue;
                    }
                };
                let result = book.mass_quote_side(seat, Side::Ask, &ask_bytes, ask_count, expiry, cursor);
                if let Err(e) = &result {
                    assert!(!is_invalid_state(e), "mass_quote_side(ask) reported corruption: {op:?}");
                }
            }
            Op::Reanchor {
                target_off,
                max_levels,
            } => {
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let anchor = book.header.anchor_tick();
                let target = anchor.wrapping_add(*target_off as i32 as u32);
                let result = book.reanchor_step(target, *max_levels % 64 + 1);
                if let Err(e) = &result {
                    assert!(!is_invalid_state(e), "reanchor_step reported corruption: {op:?}");
                }
            }
            Op::ReapExpired {
                bid,
                start_off,
                max_count,
            } => {
                let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
                let anchor = book.header.anchor_tick();
                let start = anchor.wrapping_add(*start_off as u32 % WINDOW_TICKS as u32);
                let side = if *bid { Side::Bid } else { Side::Ask };
                let result = book.reap_expired(side, start, *max_count % 64 + 1, slot);
                if let Err(e) = &result {
                    assert!(!is_invalid_state(e), "reap_expired reported corruption: {op:?}");
                }
            }
            Op::AdvanceSlot { delta } => {
                slot += *delta as u64;
            }
        }

        check_invariants(&mut data, virtual_base_vault, virtual_quote_vault);
    }
});
