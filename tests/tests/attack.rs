//! Adversarial tests. Each one is an attempt to break a stated invariant or
//! to extract value; the assertions describe what SHOULD hold, so a failure
//! here is a real finding, not a style complaint.

use clob_client::instructions::*;
use clob_client::types::Quote;
use clob_program::test_utils::*;
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
    owner: solana_pubkey::Pubkey,
    seat_index: u16,
    expiry_slot: u32,
    bids: Vec<Quote>,
    asks: Vec<Quote>,
) -> Instruction {
    MassQuote {
        owner,
        market: MARKET,
    }
    .instruction(MassQuoteInstructionArgs {
        seat_index,
        expiry_slot,
        bids,
        asks,
    })
}

fn setup(fx: &mut Fixture) {
    fx.run(claim_seat_ix(ALICE));
    fx.run(claim_seat_ix(BOB));
    fx.run(deposit(fx, 0, 100_000_000, 10_000_000_000));
    let ix = deposit(fx, 1, 100_000_000, 10_000_000_000);
    fx.run(ix);
}

/// Slab invariant: a maker has at most ONE mass-quote order per (side, tick).
/// The whole merge algorithm — and the "decrease keeps priority" rule — is
/// built on it. Attack: submit a payload whose ticks are fine on their own but
/// where post-only SLIDE collapses two quotes onto the same tick. A lossy
/// success would lie about the requested quote set, so the entire replacement
/// must fail atomically.
#[test]
fn attack_mass_quote_slide_collapses_two_quotes_onto_one_tick() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    // Bob (seat 1) rests an ask at 66_010, setting the opposing best.
    fx.run(mass_quote(BOB, 1, 0, vec![], vec![q(66_010, 100)]));

    // Alice bids 66_008 and 66_009 — neither crosses — plus 66_010 and
    // 66_011, which do and therefore slide to (best_ask - 1) = 66_009.
    fx.run_expect_failure(mass_quote(
        ALICE,
        0,
        0,
        vec![q(66_008, 10), q(66_009, 20), q(66_010, 30), q(66_011, 40)],
        vec![],
    ));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(
        book.seats[0].order_count(),
        0,
        "failed merge was not atomic"
    );
    assert_eq!(book.best_ask(), Some(66_010), "opposing quote was changed");
}

/// Moving the price window relocates where a market trades and evicts every
/// order left outside it, so it is gated on the venue's market authority.
/// A stranger naming themselves as that authority must be refused — the check
/// compares against the config, not against whoever signed.
#[test]
fn attack_reanchor_by_stranger_is_rejected() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    let mut bids: Vec<Quote> = (0..10).map(|i| q(65_990 - i, 100)).collect();
    bids.sort_by_key(|x| x.tick);
    fx.run(mass_quote(ALICE, 0, 0, bids, vec![]));

    let ix = Reanchor {
        market_authority: BOB,
        config: config_address().0,
        market: MARKET,
    }
    .instruction(ReanchorInstructionArgs {
        target_anchor: ANCHOR + 4_096,
        max_levels: 64,
    });
    fx.run_expect_failure(ix);

    // The victim's book is untouched.
    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.seats[0].order_count(), 10);
    assert_eq!(book.header.anchor_tick(), ANCHOR);
    fx.check_invariants();
}

/// The same for seats: the table is finite, so admission needs the venue's
/// seat authority. Anyone can still trade through `Swap` without one.
#[test]
fn attack_claim_seat_without_authority_is_rejected() {
    let mut fx = Fixture::new();

    let ix = ClaimSeat {
        seat_authority: BOB,
        owner: BOB,
        config: config_address().0,
        market: MARKET,
    }
    .instruction();
    fx.run_expect_failure(ix);
    fx.check_invariants();
}

/// Cancel is authorized by seat ownership. Attack: pass someone else's seat
/// index while signing as yourself.
#[test]
fn attack_cancel_other_makers_order() {
    let mut fx = Fixture::new();
    setup(&mut fx);
    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![q(66_010, 100)]));

    // Bob signs, but names alice's seat.
    let ix = Cancel {
        owner: BOB,
        market: MARKET,
    }
    .instruction(CancelInstructionArgs {
        seat_index: 0,
        node_index: 1,
        order_seq: 0,
    });
    fx.run_expect_failure(ix);

    // And bob cannot withdraw from alice's seat either.
    let ix = Withdraw {
        owner: BOB,
        market: MARKET,
        owner_base_ata: fx.bob_base_ata,
        owner_quote_ata: fx.bob_quote_ata,
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
    fx.run_expect_failure(ix);
    fx.check_invariants();
}

/// A partially filled order that is later cancelled must refund exactly the
/// UNFILLED remainder. With overflow checks off, an over-refund would be
/// silent token creation.
#[test]
fn attack_partial_fill_then_cancel_refund_exactness() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    // Alice rests 100 lots at 66_010.
    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![q(66_010, 100)]));

    // Bob takes 30 of them.
    let ix = PlaceTake {
        owner: BOB,
        market: MARKET,
    }
    .instruction(PlaceTakeInstructionArgs {
        seat_index: 1,
        side: 0,
        limit_tick: 66_010,
        max_lots: 30,
        min_out: 0,
        flags: 0,
    });
    fx.run(ix);
    fx.check_invariants();

    // Alice cancels the remaining 70 via an empty mass quote.
    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![]));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    let alice = &book.seats[0];
    assert_eq!(alice.base_locked(), 0, "locked base not fully released");
    assert_eq!(alice.quote_locked(), 0, "locked quote not fully released");
    // Started with 100_000_000 base; sold 30 lots * 1_000 atoms.
    assert_eq!(
        alice.base_free(),
        100_000_000 - 30 * BASE_LOT_SIZE,
        "base balance wrong after partial fill + cancel"
    );
}

/// Mass quote with a size DECREASE must keep FIFO priority and release exactly
/// the difference. Attack: decrease, then cancel, and check for over-refund.
#[test]
fn attack_mass_quote_decrease_then_cancel_refund() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![q(66_010, 100)]));
    let base_after_post = {
        let mut data = fx.market_data();
        let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
        book.seats[0].base_free()
    };
    assert_eq!(base_after_post, 100_000_000 - 100 * BASE_LOT_SIZE);

    // Decrease to 40 lots: 60 lots' worth of base must come back.
    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![q(66_010, 40)]));
    fx.check_invariants();

    // Cancel the rest.
    fx.run(mass_quote(ALICE, 0, 0, vec![], vec![]));
    fx.check_invariants();

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(
        book.seats[0].base_free(),
        100_000_000,
        "decrease+cancel did not restore the exact original balance"
    );
    assert_eq!(book.seats[0].base_locked(), 0);
}

/// An expired quote must be reaped exactly once — a second refund would mint
/// tokens, since seat accounting uses wrapping arithmetic. Takers are the only
/// thing that reaps now, so drive it with repeated takes over the same level.
#[test]
fn attack_double_reap_of_expired_order() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    fx.run(mass_quote(ALICE, 0, 5, vec![], vec![q(66_010, 100)]));
    fx.mollusk.warp_to_slot(10);

    for _ in 0..3 {
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
    }

    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(
        book.seats[0].base_free(),
        100_000_000,
        "expired order refunded more than once"
    );
    assert_eq!(book.seats[0].base_locked(), 0);
}

// Zero owner collides with Seat::is_free()'s sentinel; must be rejected.
#[test]
fn attack_claim_seat_rejects_zero_owner() {
    let mut fx = Fixture::new();

    let zero_owner = solana_pubkey::Pubkey::new_from_array([0u8; 32]);
    fx.run_expect_failure(claim_seat_ix(zero_owner));

    fx.run(claim_seat_ix(ALICE));
    let mut data = fx.market_data();
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
    assert_eq!(book.seats[0].owner(), ALICE.as_ref());
}
