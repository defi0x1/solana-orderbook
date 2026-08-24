//! Edge-case attacks on tick-space and crank inputs: zero prices, and
//! out-of-window scan bounds.

use clob_client::instructions::*;
use clob_program::test_utils::*;
use clob_tests::*;
use solana_account::Account;
use solana_pubkey::Pubkey;

/// `create_market` accepts `anchor_tick == 0` (it only checks `% 64 == 0`),
/// and `recenter_step` can drive any market's anchor to 0 via
/// `mid.saturating_sub(half)`. At anchor 0, tick 0 is a legal price, and
/// `price_per_lot = tick * tick_size` is zero — so the quote-budgeted match
/// used by `Swap` buys divides by zero, and a `PlaceTake` fills for free.
#[test]
fn attack_zero_tick_price() {
    // A market anchored at 0 — accepted by create_market today.
    let mollusk = mollusk();
    let (vault_authority, _) =
        Pubkey::find_program_address(&[VAULT_SEED, MARKET.as_ref()], &PROGRAM_ID);
    let rent = mollusk.sysvars.rent.minimum_balance(market_len());

    let mut fx = Fixture::new();
    // Rebuild the market account zeroed, then re-create it with anchor 0.
    let market_slot = fx
        .accounts
        .iter_mut()
        .find(|(k, _)| *k == MARKET)
        .expect("market account");
    market_slot.1 = Account::new(rent, market_len(), &PROGRAM_ID);

    let create = clob_client::instructions::CreateMarket {
        market: MARKET,
        base_mint: BASE_MINT,
        quote_mint: QUOTE_MINT,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        authority: AUTHORITY,
        config: config_address().0,
    }
    .instruction(CreateMarketInstructionArgs {
        tick_size: TICK_SIZE,
        base_lot_size: BASE_LOT_SIZE,
        anchor_tick: 0,
        fee_bps: FEE_BPS,
        max_lots_per_order: MAX_LOTS,
    });
    let created = mollusk
        .process_instruction(&create, &fx.accounts)
        .program_result
        .is_err();
    assert!(
        created,
        "create_market accepted anchor_tick = 0, making tick 0 (price 0) a \
         legal resting price on this market (F-12 regression)"
    );
    let _ = vault_authority;
}

/// The same zero-price hazard, reached WITHOUT a hostile market: a tick-0 ask
/// can only exist if the anchor is 0, but `check_or_slide` will happily slide
/// a crossing bid down to `best_ask - 1`, which is tick 0 when the best ask
/// sits at tick 1. Verifies that a zero-price resting order cannot be created
/// on a normally-anchored market.
#[test]
fn zero_price_unreachable_on_normal_market() {
    let mut fx = Fixture::new();
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

    // Tick 0 is below this market's anchor, so it must be rejected outright.
    let ix = PlaceLimit {
        owner: ALICE,
        market: MARKET,
    }
    .instruction(PlaceLimitInstructionArgs {
        seat_index: 0,
        side: 0,
        tick: 0,
        lots: 10,
        expiry_slot: 0,
        flags: 0b01,
    });
    let result = fx.mollusk.process_instruction(&ix, &fx.accounts);
    assert!(
        result.program_result.is_err(),
        "a tick-0 (price 0) order was accepted on a market anchored at {ANCHOR}"
    );
    fx.check_invariants();
}
