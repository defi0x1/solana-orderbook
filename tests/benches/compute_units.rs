//! CU benches — the numbers this whole design answers to.
//!
//! Each bench measures ONE instruction against a prepared book state. The
//! report lands in tests/benches/compute_units.md (committed; deltas reviewed
//! like code).
//!
//! The `Delta` column in that file compares against whatever run happened to
//! be on top before, which is only meaningful when both runs used the same
//! Solana CLI/Agave version — the underlying CU-metering baseline itself
//! shifts by tens-to-low-hundreds of CU across versions, independent of any
//! code change, and every instruction drifts by roughly that much even when
//! untouched. When comparing a real code change's cost, read the absolute
//! CU figures against a run on the SAME CLI version (recorded in the report
//! header), not the delta against an entry from a different one.

use clob_client::instructions::*;
use clob_client::types::Quote;
use clob_tests::*;
use mollusk_svm_bencher::MolluskComputeUnitBencher;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

type BenchCase<'a> = (&'a str, &'a Instruction, &'a [(Pubkey, Account)], u64);

fn enforce_cu_ceilings(cases: &[BenchCase<'_>]) {
    let runtime = mollusk();
    for (name, instruction, accounts, ceiling) in cases {
        let result = runtime.process_instruction(instruction, accounts);
        assert!(
            result.program_result.is_ok(),
            "CU gate {name} failed execution: {:?}",
            result.program_result
        );
        assert!(
            result.compute_units_consumed <= *ceiling,
            "CU gate {name}: consumed {} > ceiling {ceiling}",
            result.compute_units_consumed
        );
    }
}

fn q_range(start: u32, n: u32, lots: u32, step_up: bool) -> Vec<Quote> {
    (0..n)
        .map(|i| q(if step_up { start + i } else { start - i }, lots))
        .collect()
}

fn mass_quote_ix(bids: Vec<Quote>, asks: Vec<Quote>, expiry: u32) -> Instruction {
    MassQuote {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(MassQuoteInstructionArgs {
        seat_index: 0,
        expiry_slot: expiry,
        bids,
        asks,
    })
}

fn sorted(mut v: Vec<Quote>) -> Vec<Quote> {
    v.sort_by_key(|x| x.tick);
    v
}

fn main() {
    // ── State A: funded seats, empty book ────────────────────────────────
    let mut base = Fixture::new();
    base.run(claim_seat_ix(ALICE));
    base.run(claim_seat_ix(BOB));
    let ix = Deposit {
        owner: ALICE,
        market: MARKET,
        owner_base_ata: base.alice_base_ata,
        owner_quote_ata: base.alice_quote_ata,
        base_vault: base.base_vault,
        quote_vault: base.quote_vault,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(DepositInstructionArgs {
        seat_index: 0,
        base_atoms: 100_000_000,
        quote_atoms: 50_000_000_000,
    });
    base.run(ix);
    let ix = Deposit {
        owner: BOB,
        market: MARKET,
        owner_base_ata: base.bob_base_ata,
        owner_quote_ata: base.bob_quote_ata,
        base_vault: base.base_vault,
        quote_vault: base.quote_vault,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(DepositInstructionArgs {
        seat_index: 1,
        base_atoms: 100_000_000,
        quote_atoms: 50_000_000_000,
    });
    base.run(ix);
    let empty_book = base.accounts.clone();

    // ── State B: 3 resting manual bids (for cancel / place benches) ──────
    let mut with_bids = Fixture::new();
    with_bids.accounts = empty_book.clone();
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
            flags: 0b01,
        });
        with_bids.run(ix);
    }
    let bids_state = with_bids.accounts.clone();

    // ── State C: 20-level two-sided slab (for mass_quote + take benches) ─
    let bids20 = sorted(q_range(65_990, 20, 100, false));
    let asks20 = q_range(66_010, 20, 100, true);
    let mut quoted = Fixture::new();
    quoted.accounts = empty_book.clone();
    quoted.run(mass_quote_ix(bids20.clone(), asks20.clone(), 0));
    let quoted_state = quoted.accounts.clone();

    // ── Bench instructions ───────────────────────────────────────────────
    // The middle of the three resting bids: node 2 (node 0 is reserved),
    // order id 1. Cancel is O(1) via the node's own back-links — no hint.
    let cancel = Cancel {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(CancelInstructionArgs {
        seat_index: 0,
        node_index: 2,
        order_seq: 1,
    });

    let place_post = PlaceLimit {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(PlaceLimitInstructionArgs {
        seat_index: 0,
        side: 0,
        tick: 66_003,
        lots: 10,
        expiry_slot: 0,
        flags: 0b01,
    });

    let mq_fresh_40 = mass_quote_ix(bids20.clone(), asks20.clone(), 0);
    // Identical payload, new expiry: pure refresh, zero level churn.
    let mq_refresh = mass_quote_ix(bids20.clone(), asks20.clone(), 100);
    // Every tick shifted one up: full cancel+insert churn on all 40 levels.
    let mq_shift = mass_quote_ix(
        sorted(q_range(65_991, 20, 100, false)),
        q_range(66_011, 20, 100, true),
        0,
    );

    let take_1 = PlaceTake {
        owner: BOB,
        market: MARKET,
    }
    .instruction(PlaceTakeInstructionArgs {
        seat_index: 1,
        side: 0,
        limit_tick: 66_010,
        max_lots: 50,
        min_out: 0,
        flags: 0,
    });
    let take_5_levels = PlaceTake {
        owner: BOB,
        market: MARKET,
    }
    .instruction(PlaceTakeInstructionArgs {
        seat_index: 1,
        side: 0,
        limit_tick: 66_014,
        max_lots: 450,
        min_out: 0,
        flags: 0,
    });

    let swap_buy = Swap {
        user: BOB,
        market: MARKET,
        user_base_ata: base.bob_base_ata,
        user_quote_ata: base.bob_quote_ata,
        base_vault: base.base_vault,
        quote_vault: base.quote_vault,
        vault_authority: base.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(SwapInstructionArgs {
        side: 0,
        amount_in: 2_000_000_000,
        limit_tick: 66_011,
        min_out: 0,
    });

    let claim = claim_seat_ix(PAYER);

    let deposit_ix = Deposit {
        owner: ALICE,
        market: MARKET,
        owner_base_ata: base.alice_base_ata,
        owner_quote_ata: base.alice_quote_ata,
        base_vault: base.base_vault,
        quote_vault: base.quote_vault,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(DepositInstructionArgs {
        seat_index: 0,
        base_atoms: 1_000_000,
        quote_atoms: 1_000_000,
    });

    let withdraw_ix = Withdraw {
        owner: ALICE,
        market: MARKET,
        owner_base_ata: base.alice_base_ata,
        owner_quote_ata: base.alice_quote_ata,
        base_vault: base.base_vault,
        quote_vault: base.quote_vault,
        vault_authority: base.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token::ID,
    }
    .instruction(WithdrawInstructionArgs {
        seat_index: 0,
        base_atoms: 1_000_000,
        quote_atoms: 1_000_000,
    });

    // ── State D: State C plus one completed swap, so fees_accrued_quote > 0
    // ── (CollectFees is a no-op transfer at zero, so benching it needs a
    // ── real accrual to actually pay for the CPI it's meant to measure).
    let mut fee_state = Fixture::new();
    fee_state.accounts = quoted_state.clone();
    fee_state.run(swap_buy.clone());
    let authority_quote_ata = {
        let (key, account) = token_account(&AUTHORITY, &QUOTE_MINT, 0);
        fee_state.accounts.push((key, account));
        key
    };
    let fees_state = fee_state.accounts.clone();

    let collect_fees_ix = CollectFees {
        authority: AUTHORITY,
        market: MARKET,
        quote_vault: fee_state.quote_vault,
        authority_quote_ata,
        vault_authority: fee_state.vault_authority,
        quote_mint: QUOTE_MINT,
        quote_token_program: spl_token::ID,
    }
    .instruction();

    // Moving the window a quarter of its width, evicting what falls outside.
    let recenter = reanchor_ix(ANCHOR + 2_048, 64);

    // The permissionless crank scanning a 40-level book with nothing expired:
    // the fixed cost of a full-window scan.
    let reap = ReapExpired { market: MARKET }.instruction(ReapExpiredInstructionArgs {
        side: 1,
        start_tick: ANCHOR,
        max_count: 64,
    });

    // These ceilings deliberately leave a small toolchain-noise margin while
    // turning material regressions into a failing `cargo bench`.
    let cases: [BenchCase<'_>; 14] = [
        ("cancel", &cancel, &bids_state, 280),
        ("reanchor_2048_ticks", &recenter, &quoted_state, 12_800),
        ("reap_expired_scan_none", &reap, &quoted_state, 2_000),
        ("place_limit_post", &place_post, &bids_state, 650),
        ("mass_quote_40_fresh", &mq_fresh_40, &empty_book, 14_500),
        (
            "mass_quote_40_refresh_expiry_only",
            &mq_refresh,
            &quoted_state,
            4_600,
        ),
        ("mass_quote_40_shift_all", &mq_shift, &quoted_state, 5_500),
        ("place_take_1_level", &take_1, &quoted_state, 820),
        ("place_take_5_levels", &take_5_levels, &quoted_state, 2_000),
        ("swap_buy_2_levels", &swap_buy, &quoted_state, 24_100),
        ("claim_seat", &claim, &empty_book, 700),
        ("deposit", &deposit_ix, &empty_book, 15_450),
        ("withdraw", &withdraw_ix, &empty_book, 15_450),
        ("collect_fees", &collect_fees_ix, &fees_state, 8_200),
    ];
    enforce_cu_ceilings(&cases);

    MolluskComputeUnitBencher::new(mollusk())
        .bench(("cancel", &cancel, &bids_state))
        .bench(("reanchor_2048_ticks", &recenter, &quoted_state))
        .bench(("reap_expired_scan_none", &reap, &quoted_state))
        .bench(("place_limit_post", &place_post, &bids_state))
        .bench(("mass_quote_40_fresh", &mq_fresh_40, &empty_book))
        .bench((
            "mass_quote_40_refresh_expiry_only",
            &mq_refresh,
            &quoted_state,
        ))
        .bench(("mass_quote_40_shift_all", &mq_shift, &quoted_state))
        .bench(("place_take_1_level", &take_1, &quoted_state))
        .bench(("place_take_5_levels", &take_5_levels, &quoted_state))
        .bench(("swap_buy_2_levels", &swap_buy, &quoted_state))
        .bench(("claim_seat", &claim, &empty_book))
        .bench(("deposit", &deposit_ix, &empty_book))
        .bench(("withdraw", &withdraw_ix, &empty_book))
        .bench(("collect_fees", &collect_fees_ix, &fees_state))
        .must_pass(true)
        .out_dir("benches/")
        .execute();
}
