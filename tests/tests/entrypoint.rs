//! Entrypoint shape handling.
//!
//! `program/src/entrypoint.rs` reimplements account deserialization for the
//! maker shape (2 accounts, the only hot shape): it probes the loader's
//! input buffer at hand-computed offsets and dispatches directly, while
//! everything else — takers, cranks, admin, seat — goes through pinocchio's
//! `deserialize` in a `#[cold]` fallback.
//!
//! The obvious worry is "two implementations that must agree". These tests pin
//! down why that worry is smaller than it looks: every handler destructures
//! its accounts with an exact-arity slice pattern (`let [owner, market] =
//! accounts else { return Err(NotEnoughAccountKeys) }`), so a maker call with
//! a trailing account is REJECTED rather than silently accepted on the generic
//! path. Combined with the shape probe requiring an exact account count, a
//! valid maker call always takes the fast path, and the generic path only
//! ever sees maker input that goes on to fail. There is one live
//! implementation, not two.
//!
//! That strict arity is also a security property in its own right — programs
//! that ignore trailing accounts have been the root of real exploits — so it
//! is pinned here rather than left as an accident of the pattern match.

use clob_client::instructions::*;
use clob_tests::*;
use solana_instruction::{AccountMeta, Instruction};

fn with_extra_account(mut ix: Instruction) -> Instruction {
    ix.accounts.push(AccountMeta::new_readonly(PAYER, false));
    ix
}

fn setup(fx: &mut Fixture) {
    fx.run(claim_seat_ix(ALICE));
    let ix = Deposit {
        owner: ALICE,
        market: MARKET,
        owner_base_ata: fx.alice_base_ata,
        owner_quote_ata: fx.alice_quote_ata,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(DepositInstructionArgs {
        seat_index: 0,
        base_atoms: 100_000_000,
        quote_atoms: 10_000_000_000,
    });
    fx.run(ix);
}

fn mass_quote_ix() -> Instruction {
    MassQuote {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(MassQuoteInstructionArgs {
        seat_index: 0,
        expiry_slot: 0,
        bids: vec![q(65_988, 10), q(65_990, 20)],
        asks: vec![q(66_010, 30), q(66_012, 40)],
    })
}

/// Strict arity: a maker instruction with one extra account is rejected, not
/// silently executed. This is what makes the generic path unreachable for
/// valid maker input — and it is the property that stops "pass an extra
/// account to dodge the fast path" from being a way to reach a second,
/// less-tested code path.
#[test]
fn extra_account_is_rejected_for_maker_instructions() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    // The instruction succeeds in its exact-arity form...
    fx.run(mass_quote_ix());
    fx.check_invariants();

    // ...and is rejected with a trailing account.
    fx.run_expect_failure(with_extra_account(mass_quote_ix()));

    fx.check_invariants();
}

/// Same strict arity on the cold path: the authority-gated reanchor and the
/// one-account permissionless reap both reject a trailing account.
#[test]
fn extra_account_is_rejected_for_cranks() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    let recenter = reanchor_ix(ANCHOR, 8);
    fx.run(recenter.clone());
    fx.run_expect_failure(with_extra_account(recenter));

    let reap = ReapExpired { market: MARKET }.instruction(ReapExpiredInstructionArgs {
        side: 0,
        start_tick: ANCHOR,
        max_count: 8,
    });
    fx.run(reap.clone());
    fx.run_expect_failure(with_extra_account(reap));

    fx.check_invariants();
}

/// A maker call whose two accounts are the SAME account fails the fast path's
/// non-duplicate probe and drops to the generic path, which must reject it —
/// a market account is not a valid seat owner, and it is not a signer.
#[test]
fn duplicate_accounts_fall_through_and_are_rejected() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    let mut ix = mass_quote_ix();
    ix.accounts[0] = AccountMeta::new(MARKET, false);

    fx.run_expect_failure(ix);
    fx.check_invariants();
}

/// The fast path reads the discriminator out of the raw buffer itself, so a
/// 1-byte payload exercises its `ix_len >= 1` guard, and a 0-byte payload the
/// path where no discriminator exists at all. Both must fail cleanly rather
/// than reading past the instruction data.
#[test]
fn truncated_payload_is_rejected() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    let accounts = vec![
        AccountMeta::new_readonly(ALICE, true),
        AccountMeta::new(MARKET, false),
    ];

    // Discriminator only, no arguments.
    fx.run_expect_failure(Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts.clone(),
        data: vec![MASS_QUOTE_DISCRIMINATOR],
    });

    // No instruction data at all.
    fx.run_expect_failure(Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts.clone(),
        data: vec![],
    });

    // Unknown discriminator.
    fx.run_expect_failure(Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: vec![0xEE],
    });

    fx.check_invariants();
}

/// A non-maker discriminator sent with the maker shape (2 accounts) and a
/// maker discriminator sent with a 1-account shape must both be rejected:
/// the probe matches on account count and discriminator, so these land in
/// the generic path and must fail there on arity.
#[test]
fn mismatched_shape_and_discriminator_is_rejected() {
    let mut fx = Fixture::new();
    setup(&mut fx);

    // Reanchor (3 accounts) sent with 2 accounts.
    fx.run_expect_failure(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(ALICE, true),
            AccountMeta::new(MARKET, false),
        ],
        data: {
            let mut d = vec![REANCHOR_DISCRIMINATOR];
            d.extend_from_slice(&8u32.to_le_bytes());
            d
        },
    });

    // MassQuote (maker, 2 accounts) sent with 1 account.
    fx.run_expect_failure(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![AccountMeta::new(MARKET, false)],
        data: {
            let mut d = vec![MASS_QUOTE_DISCRIMINATOR];
            d.extend_from_slice(&0u16.to_le_bytes()); // seat
            d.extend_from_slice(&0u32.to_le_bytes()); // expiry
            d.push(0); // bid count
            d.push(0); // ask count
            d
        },
    });

    fx.check_invariants();
}
