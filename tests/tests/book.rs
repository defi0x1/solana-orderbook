//! Integration tests: every flow goes through the Codama-generated client
//! against the real SBF build, with the standing invariants checked after
//! every scenario.

use clob_client::instructions::*;
use clob_client::snapshot::{MarketSnapshot, OrderSide, SnapshotError};
use clob_client::types::Quote;
use clob_program::test_utils::{BookRefMut, MARKET_VERSION, WINDOW_MASK, WINDOW_TICKS};
use clob_tests::*;
use solana_instruction::Instruction;

fn deposit(fx: &Fixture, seat_index: u16, base: u64, quote: u64) -> Instruction {
    let (owner_base_ata, owner_quote_ata, owner) = if seat_index == 0 {
        (fx.alice_base_ata, fx.alice_quote_ata, ALICE)
    } else {
        (fx.bob_base_ata, fx.bob_quote_ata, BOB)
    };
    Deposit {
        owner,
        market: MARKET,
        owner_base_ata,
        owner_quote_ata,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(DepositInstructionArgs {
        seat_index,
        base_atoms: base,
        quote_atoms: quote,
    })
}

fn mass_quote(
    seat_index: u16,
    expiry_slot: u32,
    bids: Vec<Quote>,
    asks: Vec<Quote>,
) -> Instruction {
    MassQuote {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(MassQuoteInstructionArgs {
        seat_index,
        expiry_slot,
        bids,
        asks,
    })
}

/// Standard two-seat setup: alice (seat 0, maker) and bob (seat 1, taker),
/// both funded.
fn setup_seats(fx: &mut Fixture) {
    fx.run(claim_seat_ix(ALICE));
    fx.run(claim_seat_ix(BOB));
    fx.run(deposit(fx, 0, 100_000_000, 10_000_000_000));
    let ix = deposit(fx, 1, 100_000_000, 10_000_000_000);
    fx.run(ix);
}

#[test]
fn create_market() {
    let fx = Fixture::new();
    let data = fx.market_data();
    let header = clob_client::accounts::Market::from_bytes(&data[..320]).unwrap();
    assert_eq!(header.version, MARKET_VERSION);
    assert_eq!(header.tick_size, TICK_SIZE);
    assert_eq!(header.base_lot_size, BASE_LOT_SIZE);
    assert_eq!(header.anchor_tick, ANCHOR);
    assert_eq!(header.fee_bps, FEE_BPS);
    assert_eq!(header.order_pool_capacity, CAPACITY as u32);
    // Node 0 is the reserved NIL node, so one fewer node is allocatable.
    assert_eq!(header.free_count, CAPACITY as u32 - 1);
    assert_eq!(header.seq, 1);
    assert_eq!(header.config, config_address().0);
    fx.check_invariants();
}

#[test]
fn full_market_snapshot_decoder_matches_live_book() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);
    fx.run(mass_quote(0, 0, vec![q(65_990, 10)], vec![q(66_010, 20)]));

    let data = fx.market_data();
    let snapshot = MarketSnapshot::decode(&data).unwrap();
    assert_eq!(snapshot.levels.len(), WINDOW_TICKS);
    assert_eq!(snapshot.seats.len(), 128);
    assert_eq!(snapshot.orders.len(), CAPACITY);
    assert!(snapshot.bid_bitmap.contains(65_990));
    assert!(snapshot.ask_bitmap.contains(66_010));

    let orders: Vec<_> = snapshot.live_orders().collect();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].maker, 0);
    assert_eq!(orders[0].side, OrderSide::Bid);
    assert_eq!(orders[1].side, OrderSide::Ask);

    assert!(matches!(
        MarketSnapshot::decode(&data[..data.len() - 1]),
        Err(SnapshotError::LengthMismatch { .. })
    ));
}

#[test]
fn seat_deposit_withdraw() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);
    fx.check_invariants();

    let ix = Withdraw {
        owner: ALICE,
        market: MARKET,
        owner_base_ata: fx.alice_base_ata,
        owner_quote_ata: fx.alice_quote_ata,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        vault_authority: fx.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(WithdrawInstructionArgs {
        seat_index: 0,
        base_atoms: u64::MAX,
        quote_atoms: u64::MAX,
    });
    fx.run(ix);
    fx.check_invariants();

    // Seat 0 is empty again: closing must succeed.
    let ix = CloseSeat {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(CloseSeatInstructionArgs { seat_index: 0 });
    fx.run(ix);
    fx.check_invariants();
}

#[test]
fn place_and_cancel() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    // Three post-only bids; nodes allocate in order 1, 2, 3 (node 0 is the
    // reserved NIL node) with order ids 0, 1, 2.
    for (i, tick) in [66_000u32, 66_001, 66_002].into_iter().enumerate() {
        let ix = PlaceLimit {
            owner: ALICE,
            market: MARKET,
        }
        .instruction(PlaceLimitInstructionArgs {
            seat_index: 0,
            side: 0,
            tick,
            lots: 10 + i as u32,
            expiry_slot: 0,
            flags: 0b01, // post_only
        });
        fx.run(ix);
    }
    fx.check_invariants();

    // Cancel the middle order (node 2, order id 1) — O(1), no hint needed.
    let ix = Cancel {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(CancelInstructionArgs {
        seat_index: 0,
        node_index: 2,
        order_seq: 1,
    });
    fx.run(ix);
    fx.check_invariants();

    // Cancelling it again must fail (OrderNotFound).
    let ix = Cancel {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(CancelInstructionArgs {
        seat_index: 0,
        node_index: 2,
        order_seq: 1,
    });
    fx.run_expect_failure(ix);

    // Cancel everything; the book must be empty.
    fx.run(mass_quote(0, 0, vec![], vec![]));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert!(book.best_bid().is_none());
    assert!(book.best_ask().is_none());
}

#[test]
fn cancel_accepts_64_bit_generation_and_skips_free_marker() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    {
        let market = &mut fx
            .accounts
            .iter_mut()
            .find(|(key, _)| *key == MARKET)
            .unwrap()
            .1;
        let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut market.data) };
        book.header.set_next_order_id_for_test(u64::MAX - 1);
    }

    for tick in [66_000, 66_001] {
        fx.run(
            PlaceLimit {
                owner: ALICE,
                market: MARKET,
            }
            .instruction(PlaceLimitInstructionArgs {
                seat_index: 0,
                side: 0,
                tick,
                lots: 10,
                expiry_slot: 0,
                flags: 0b01,
            }),
        );
    }

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.pool[1].seq(), u64::MAX - 1);
    assert_eq!(book.pool[2].seq(), 0, "free marker was used as a live id");

    fx.run(
        Cancel {
            owner: ALICE,
            market: MARKET,
        }
        .instruction(CancelInstructionArgs {
            seat_index: 0,
            node_index: 1,
            order_seq: u64::MAX - 1,
        }),
    );
    fx.check_invariants();
}

#[test]
fn mass_quote_and_refresh() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    let bids: Vec<Quote> = (0..10).map(|i| q(65_990 - i, 100)).collect();
    let asks: Vec<Quote> = (0..10).map(|i| q(66_010 + i, 100)).collect();
    let mut bids_sorted = bids.clone();
    bids_sorted.sort_by_key(|x| x.tick);
    fx.run(mass_quote(0, 0, bids_sorted.clone(), asks.clone()));
    fx.check_invariants();

    {
        let mut data = fx.market_data();
        let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
        assert_eq!(book.best_bid(), Some(65_990));
        assert_eq!(book.best_ask(), Some(66_010));
    }

    // Refresh shifted one tick up: merge should cancel the dropped edge,
    // keep the overlap, insert the new edge.
    let bids2: Vec<Quote> = (0..10).map(|i| q(65_991 - i, 100)).collect();
    let asks2: Vec<Quote> = (0..10).map(|i| q(66_011 + i, 100)).collect();
    let mut bids2_sorted = bids2.clone();
    bids2_sorted.sort_by_key(|x| x.tick);
    fx.run(mass_quote(0, 0, bids2_sorted, asks2));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.best_bid(), Some(65_991));
    assert_eq!(book.best_ask(), Some(66_011));

    // Empty payload cancels the whole slab.
    fx.run(mass_quote(0, 0, vec![], vec![]));
    fx.check_invariants();
    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert!(book.best_bid().is_none());
    assert!(book.best_ask().is_none());
}

#[test]
fn mass_quote_can_flip_a_full_seat_from_asks_to_bids() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    let asks: Vec<Quote> = (0..128).map(|i| q(67_000 + i, 1)).collect();
    fx.run(mass_quote(0, 0, vec![], asks));
    fx.check_invariants();

    // The final set is legal even though the bid-first merge temporarily
    // holds both the old asks and the replacement bids.
    let bids: Vec<Quote> = (0..128).map(|i| q(65_000 + i, 1)).collect();
    fx.run(mass_quote(0, 0, bids, vec![]));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.seats[0].order_count(), 128);
    assert_eq!(book.best_bid(), Some(65_127));
    assert!(book.best_ask().is_none());
}

#[test]
fn zero_lot_place_limit_cancels_without_blocking_swap() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    for tick in [66_010, 66_011] {
        fx.run(
            PlaceLimit {
                owner: ALICE,
                market: MARKET,
            }
            .instruction(PlaceLimitInstructionArgs {
                seat_index: 0,
                side: 1,
                tick,
                lots: 10,
                expiry_slot: 0,
                flags: 0b01,
            }),
        );
    }

    // flags=0 exercises the crossing-capable PlaceLimit path: zero size is
    // still an exact cancellation, not a zero-lot taker or resting node.
    fx.run(
        PlaceLimit {
            owner: ALICE,
            market: MARKET,
        }
        .instruction(PlaceLimitInstructionArgs {
            seat_index: 0,
            side: 1,
            tick: 66_010,
            lots: 0,
            expiry_slot: 0,
            flags: 0,
        }),
    );
    fx.check_invariants();

    let notional = 66_011 * TICK_SIZE;
    let fee = notional * FEE_BPS as u64 / 10_000;
    fx.run(
        Swap {
            user: BOB,
            market: MARKET,
            user_base_ata: fx.bob_base_ata,
            user_quote_ata: fx.bob_quote_ata,
            base_vault: fx.base_vault,
            quote_vault: fx.quote_vault,
            vault_authority: fx.vault_authority,
            base_mint: BASE_MINT,
            quote_mint: QUOTE_MINT,
            base_token_program: spl_token::ID,
            quote_token_program: spl_token::ID,
        }
        .instruction(SwapInstructionArgs {
            side: 0,
            // One atom of headroom for the conservative inverse fee bound.
            amount_in: notional + fee + 1,
            limit_tick: 66_011,
            min_out: BASE_LOT_SIZE,
        }),
    );
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.best_ask(), Some(66_011));
    assert_eq!(book.levels[(66_011 & WINDOW_MASK) as usize].total_lots(), 9);
}

#[test]
fn take_across_levels() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    // Alice posts three ask levels: 100 lots @ 66_010, 66_011, 66_012.
    let asks: Vec<Quote> = (0..3).map(|i| q(66_010 + i, 100)).collect();
    fx.run(mass_quote(0, 0, vec![], asks));

    // Bob lifts 250 lots across all three levels.
    let ix = PlaceTake {
        owner: BOB,
        market: MARKET,
    }
    .instruction(PlaceTakeInstructionArgs {
        seat_index: 1,
        side: 0, // buy
        limit_tick: 66_012,
        max_lots: 250,
        min_out: 250 * BASE_LOT_SIZE,
        flags: 0,
    });
    fx.run(ix);
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    // 100 @ 66_010 + 100 @ 66_011 + 50 @ 66_012.
    let notional = (100 * 66_010u64 + 100 * 66_011 + 50 * 66_012) * TICK_SIZE;
    let fee = notional * FEE_BPS as u64 / 10_000;
    assert_eq!(book.header.fees_accrued_quote(), fee);
    // Bob paid notional + fee, received 250 lots of base.
    let bob = &book.seats[1];
    assert_eq!(bob.base_free(), 100_000_000 + 250 * BASE_LOT_SIZE);
    assert_eq!(bob.quote_free(), 10_000_000_000 - notional - fee);
    // Alice received the full notional, gave up the base.
    let alice = &book.seats[0];
    assert_eq!(alice.quote_free(), 10_000_000_000 + notional);
    // The half-filled level survives with 50 lots.
    assert_eq!(book.best_ask(), Some(66_012));
}

#[test]
fn swap_buy_and_sell() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    // Alice quotes both sides.
    fx.run(mass_quote(0, 0, vec![q(65_990, 100)], vec![q(66_010, 100)]));

    let bob_quote_before = fx.token_amount(&fx.bob_quote_ata);
    let bob_base_before = fx.token_amount(&fx.bob_base_ata);

    // Bob buys 50 lots via swap: exact-in on the quote side.
    let notional = 50 * 66_010u64 * TICK_SIZE;
    let fee = notional * FEE_BPS as u64 / 10_000;
    let ix = Swap {
        user: BOB,
        market: MARKET,
        user_base_ata: fx.bob_base_ata,
        user_quote_ata: fx.bob_quote_ata,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        vault_authority: fx.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(SwapInstructionArgs {
        side: 0,
        amount_in: notional + fee,
        limit_tick: 66_010,
        min_out: 50 * BASE_LOT_SIZE,
    });
    fx.run(ix);
    fx.check_invariants();

    assert_eq!(
        fx.token_amount(&fx.bob_quote_ata),
        bob_quote_before - notional - fee
    );
    assert_eq!(
        fx.token_amount(&fx.bob_base_ata),
        bob_base_before + 50 * BASE_LOT_SIZE
    );

    // And sells 30 lots back into alice's bid.
    let ix = Swap {
        user: BOB,
        market: MARKET,
        user_base_ata: fx.bob_base_ata,
        user_quote_ata: fx.bob_quote_ata,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        vault_authority: fx.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(SwapInstructionArgs {
        side: 1,
        amount_in: 30 * BASE_LOT_SIZE,
        limit_tick: 65_990,
        min_out: 0,
    });
    fx.run(ix);
    fx.check_invariants();
}

#[test]
fn expired_quotes_are_reaped_not_filled() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    // Alice quotes with expiry at slot 5.
    fx.run(mass_quote(0, 5, vec![], vec![q(66_010, 100)]));

    // Past the expiry, bob's take finds nothing: the quote is reaped mid-walk
    // (alice refunded), and with min_out = 0 the take succeeds with 0 fills.
    fx.mollusk.warp_to_slot(10);
    let ix = PlaceTake {
        owner: BOB,
        market: MARKET,
    }
    .instruction(PlaceTakeInstructionArgs {
        seat_index: 1,
        side: 0,
        limit_tick: 66_012,
        max_lots: 50,
        min_out: 0,
        flags: 0,
    });
    fx.run(ix);
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert!(book.best_ask().is_none(), "expired quote not reaped");
    let alice = &book.seats[0];
    assert_eq!(alice.base_locked(), 0, "expired quote still locked");
}

#[test]
fn reap_expired_crank_is_permissionless() {
    let mut fx = Fixture::new();
    setup_seats(&mut fx);

    // Alice quotes both sides with expiry at slot 5; no taker ever crosses.
    fx.run(mass_quote(0, 5, vec![q(65_990, 100)], vec![q(66_010, 100)]));
    fx.mollusk.warp_to_slot(10);

    // The crank needs no signer: one account, the market.
    for side in [0u8, 1] {
        let ix = ReapExpired { market: MARKET }.instruction(ReapExpiredInstructionArgs {
            side,
            start_tick: ANCHOR,
            max_count: 64,
        });
        fx.run(ix);
        fx.check_invariants();
    }

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert!(book.best_bid().is_none(), "expired bid not reaped");
    assert!(book.best_ask().is_none(), "expired ask not reaped");
    let alice = &book.seats[0];
    assert_eq!(alice.base_locked(), 0, "reap did not refund base");
    assert_eq!(alice.quote_locked(), 0, "reap did not refund quote");
    assert_eq!(alice.order_count(), 0, "reaped orders still counted");
}
