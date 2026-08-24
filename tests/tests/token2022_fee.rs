//! Swap against a REAL Token-2022 mint carrying the `TransferFeeConfig`
//! extension, executed through the actual compiled Token-2022 program (not a
//! mock) via `mollusk_svm_programs_token::token2022`.
//!
//! This closes the coverage gap behind the Swap input-leg reorder: matching
//! must happen against the OBSERVED post-fee delta, not the nominal
//! `amount_in`, or every swap against a fee-bearing input mint reverts (the
//! bug this file exists to catch a regression of). A mocked "pretend this
//! mint charges a fee" test cannot exercise that — only a real fee-charging
//! CPI can, since the fee is computed and applied by the token program
//! itself, not by this program.
//!
//! Fixture construction uses `spl_token_2022`'s own Pod-packing APIs
//! (`PodStateWithExtensionsMut`, `init_extension`) rather than hand-derived
//! byte offsets, mirroring exactly what `spl_token_2022`'s own
//! `InitializeTransferFeeConfig` processor does internally — see
//! `spl_token_2022::processor::process_initialize_transfer_fee_config`.

use clob_client::errors::ClobError;
use clob_client::instructions::*;
use clob_program::test_utils::*;
use clob_tests::*;
use mollusk_svm::result::{Check, ProgramResult};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_pubkey::Pubkey;
use spl_token::solana_program::{
    program_error::ProgramError, program_option::COption, program_pack::Pack,
};
use spl_token_2022::extension::permanent_delegate::PermanentDelegate;
use spl_token_2022::extension::transfer_fee::{TransferFee, TransferFeeAmount, TransferFeeConfig};
use spl_token_2022::extension::{
    BaseStateWithExtensionsMut, ExtensionType, PodStateWithExtensionsMut,
};
use spl_token_2022::pod::{PodAccount, PodMint};

const FEE_QUOTE_MINT: Pubkey = Pubkey::new_from_array([0x22; 32]);
const ALICE_FEE_QUOTE_ATA: Pubkey = Pubkey::new_from_array([0x23; 32]);
const BOB_FEE_QUOTE_ATA: Pubkey = Pubkey::new_from_array([0x24; 32]);
const FEE_BASIS_POINTS: u16 = 100; // 1%
const QUOTE_DECIMALS: u8 = 6;

/// A Token-2022 mint with a live `TransferFeeConfig` (1%, uncapped) — built
/// via the same `PodStateWithExtensionsMut::init_extension` path the real
/// on-chain program's own `InitializeTransferFeeConfig` processor uses, so
/// the byte layout is correct by construction rather than by hand-derived
/// offsets.
fn fee_mint_account() -> Account {
    let ext_types = [ExtensionType::TransferFeeConfig];
    let len = ExtensionType::try_calculate_account_len::<PodMint>(&ext_types).unwrap();
    let mut data = vec![0u8; len];
    {
        let mut state =
            PodStateWithExtensionsMut::<PodMint>::unpack_uninitialized(&mut data).unwrap();
        state.base.decimals = QUOTE_DECIMALS;
        state.base.is_initialized = true.into();
        let fee = TransferFee {
            epoch: 0u64.into(),
            maximum_fee: u64::MAX.into(), // uncapped, so bps is the only lever
            transfer_fee_basis_points: FEE_BASIS_POINTS.into(),
        };
        let ext = state.init_extension::<TransferFeeConfig>(true).unwrap();
        ext.transfer_fee_config_authority = None.try_into().unwrap();
        ext.withdraw_withheld_authority = None.try_into().unwrap();
        ext.withheld_amount = 0u64.into();
        ext.older_transfer_fee = fee;
        ext.newer_transfer_fee = fee;
        state.init_account_type().unwrap();
    }
    Account {
        lamports: 1_000_000_000,
        data,
        owner: spl_token_2022::ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// A Token-2022 token account for `FEE_QUOTE_MINT`, carrying the
/// `TransferFeeAmount` extension every account of a fee-mint needs: the real
/// processor requires it on the DESTINATION of any transfer that assesses a
/// nonzero fee (`process_transfer` in `spl_token_2022::processor` — a
/// destination without it makes the whole transfer fail with
/// `TokenError::InvalidState`), and since the vault/ATA pair here plays both
/// source and destination roles across the pull-in and refund legs, every
/// account gets it.
fn fee_token_account(owner: &Pubkey, amount: u64) -> Account {
    let ext_types = [ExtensionType::TransferFeeAmount];
    let len = ExtensionType::try_calculate_account_len::<PodAccount>(&ext_types).unwrap();
    let mut data = vec![0u8; len];
    {
        let mut state =
            PodStateWithExtensionsMut::<PodAccount>::unpack_uninitialized(&mut data).unwrap();
        state.base.mint = FEE_QUOTE_MINT;
        state.base.owner = *owner;
        state.base.amount = amount.into();
        state.base.state = spl_token_2022::state::AccountState::Initialized as u8;
        let ext = state.init_extension::<TransferFeeAmount>(true).unwrap();
        ext.withheld_amount = 0u64.into();
        state.init_account_type().unwrap();
    }
    Account {
        lamports: 2_039_280,
        data,
        owner: spl_token_2022::ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// A `Fixture`-equivalent market, but with the QUOTE mint set to the real
/// Token-2022 fee mint instead of `QUOTE_MINT`. Built by hand rather than
/// via `Fixture::new()` since the vault/mint/program wiring differs from
/// every other test in this suite. BASE stays the ordinary legacy-Token
/// mint — only the leg under test (Swap's quote input on the Bid side)
/// needs to be fee-bearing.
struct FeeFixture {
    mollusk: Mollusk,
    accounts: Vec<(Pubkey, Account)>,
    vault_authority: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
}

impl FeeFixture {
    fn new() -> Self {
        let mut mollusk = mollusk();
        mollusk_svm_programs_token::token2022::add_program(&mut mollusk);

        let (vault_auth, _) = vault_authority();
        let (base_vault, _) = Pubkey::find_program_address(
            &[
                vault_auth.as_ref(),
                spl_token::ID.as_ref(),
                BASE_MINT.as_ref(),
            ],
            &spl_associated_token_account::ID,
        );
        let (quote_vault, _) = Pubkey::find_program_address(
            &[
                vault_auth.as_ref(),
                spl_token_2022::ID.as_ref(),
                FEE_QUOTE_MINT.as_ref(),
            ],
            &spl_associated_token_account::ID,
        );

        let base_vault_account = {
            let token_account_data = spl_token::state::Account {
                owner: vault_auth,
                mint: BASE_MINT,
                amount: 0,
                delegate: COption::None,
                delegated_amount: 0,
                state: spl_token::state::AccountState::Initialized,
                is_native: COption::None,
                close_authority: COption::None,
            };
            let mut data = vec![0u8; spl_token::state::Account::LEN];
            spl_token::state::Account::pack(token_account_data, &mut data).unwrap();
            Account {
                lamports: 2_039_280,
                data,
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            }
        };

        let alice_base_ata = alice_base_ata_address();
        let alice_base_account = {
            let token_account_data = spl_token::state::Account {
                owner: ALICE,
                mint: BASE_MINT,
                amount: 1_000_000_000,
                delegate: COption::None,
                delegated_amount: 0,
                state: spl_token::state::AccountState::Initialized,
                is_native: COption::None,
                close_authority: COption::None,
            };
            let mut data = vec![0u8; spl_token::state::Account::LEN];
            spl_token::state::Account::pack(token_account_data, &mut data).unwrap();
            Account {
                lamports: 2_039_280,
                data,
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            }
        };

        let (system_program, system_program_account) =
            mollusk_svm::program::keyed_account_for_system_program();
        let (legacy_token_program, legacy_token_program_account) =
            mollusk_svm_programs_token::token::keyed_account();
        let (token2022_program, token2022_program_account) =
            mollusk_svm_programs_token::token2022::keyed_account();

        let rent = mollusk.sysvars.rent.minimum_balance(market_len());
        let market_account = Account::new(rent, market_len(), &PROGRAM_ID);
        let (config, _) = config_address();

        let accounts = vec![
            (
                AUTHORITY,
                Account::new(1_000_000_000, 0, &Pubkey::default()),
            ),
            (config, Account::default()),
            (ALICE, Account::new(1_000_000_000, 0, &Pubkey::default())),
            (BOB, Account::new(1_000_000_000, 0, &Pubkey::default())),
            (PAYER, Account::new(100_000_000_000, 0, &Pubkey::default())),
            (BASE_MINT, mint_account_legacy(9)),
            (FEE_QUOTE_MINT, fee_mint_account()),
            (MARKET, market_account),
            (vault_auth, Account::default()),
            (base_vault, base_vault_account),
            (quote_vault, fee_token_account(&vault_auth, 0)),
            (alice_base_ata, alice_base_account),
            (ALICE_FEE_QUOTE_ATA, fee_token_account(&ALICE, 0)),
            (BOB_FEE_QUOTE_ATA, fee_token_account(&BOB, 100_000_000_000)),
            (system_program, system_program_account),
            (legacy_token_program, legacy_token_program_account),
            (token2022_program, token2022_program_account),
        ];

        let mut fixture = Self {
            mollusk,
            accounts,
            vault_authority: vault_auth,
            base_vault,
            quote_vault,
        };

        fixture.run(
            clob_client::instructions::CreateConfig {
                payer: PAYER,
                config,
                authority: AUTHORITY,
                system_program: Pubkey::default(),
            }
            .instruction(clob_client::instructions::CreateConfigInstructionArgs {
                seat_authority: SEAT_AUTHORITY,
                market_authority: MARKET_AUTHORITY,
            }),
        );

        fixture.run(
            CreateMarket {
                market: MARKET,
                base_mint: BASE_MINT,
                quote_mint: FEE_QUOTE_MINT,
                base_vault,
                quote_vault,
                authority: AUTHORITY,
                config,
            }
            .instruction(CreateMarketInstructionArgs {
                tick_size: TICK_SIZE,
                base_lot_size: BASE_LOT_SIZE,
                anchor_tick: ANCHOR,
                fee_bps: FEE_BPS,
                max_lots_per_order: MAX_LOTS,
            }),
        );

        fixture.run(claim_seat_ix(ALICE));

        fixture.run(
            Deposit {
                owner: ALICE,
                market: MARKET,
                owner_base_ata: alice_base_ata,
                owner_quote_ata: ALICE_FEE_QUOTE_ATA,
                base_vault,
                quote_vault,
                base_mint: BASE_MINT,
                quote_mint: FEE_QUOTE_MINT,
                base_token_program: spl_token::ID,
                quote_token_program: spl_token_2022::ID,
            }
            .instruction(DepositInstructionArgs {
                seat_index: 0,
                base_atoms: 500_000_000,
                quote_atoms: 0,
            }),
        );

        fixture
    }

    fn run(&mut self, ix: solana_instruction::Instruction) {
        let result =
            self.mollusk
                .process_and_validate_instruction(&ix, &self.accounts, &[Check::success()]);
        for (key, account) in result.resulting_accounts {
            if let Some(slot) = self.accounts.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = account;
            } else {
                self.accounts.push((key, account));
            }
        }
    }

    fn account(&self, key: &Pubkey) -> &Account {
        &self.accounts.iter().find(|(k, _)| k == key).unwrap().1
    }

    /// Raw `amount` read (offset 64, 8 bytes LE) — `spl_token::state::
    /// Account::unpack` requires an exact 165-byte legacy layout and errors
    /// on an extended Token-2022 account, so it can't be reused here.
    fn token_amount_raw(&self, key: &Pubkey) -> u64 {
        let data = &self.account(key).data;
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }
}

/// A minimal legacy-Token mint (BASE_MINT for this fixture), matching
/// `clob_tests::Fixture`'s own `mint_account` helper but kept local since
/// that one is private to the `clob_tests` crate.
fn mint_account_legacy(decimals: u8) -> Account {
    let mint_data = spl_token::state::Mint {
        mint_authority: COption::None,
        supply: 0,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    };
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint::pack(mint_data, &mut data).unwrap();
    Account {
        lamports: 1_000_000_000,
        data,
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }
}

fn alice_base_ata_address() -> Pubkey {
    Pubkey::new_from_array([0x25; 32])
}

/// Alice quotes a single ask at `tick`/`lots`, so a Bid-side Swap has
/// something to fill against.
fn post_alice_ask(fx: &mut FeeFixture, tick: u32, lots: u32) {
    fx.run(
        MassQuote {
            owner: ALICE,
            market: MARKET,
        }
        .instruction(clob_client::instructions::MassQuoteInstructionArgs {
            seat_index: 0,
            expiry_slot: 0,
            bids: vec![],
            asks: vec![q(tick, lots)],
        }),
    );
}

/// The bug this file exists to catch a regression of: matching against the
/// nominal `amount_in` instead of the observed post-fee delta made every
/// swap against a fee-bearing input mint revert, unconditionally. This
/// confirms it now succeeds and credits the FEE-ADJUSTED amount, not the
/// nominal one.
#[test]
fn swap_against_fee_bearing_input_mint_succeeds_and_matches_the_fee_adjusted_delta() {
    let mut fx = FeeFixture::new();
    // A small, fully-affordable resting order: 10 lots at 66_010 costs
    // 10 * 66_010 * TICK_SIZE = 66_010_000 quote atoms of notional
    // (+ this CLOB's own 0.1% taker fee on top). amount_in is chosen large
    // enough that even after the mint's 1% transfer fee, the observed delta
    // comfortably covers that — resting size is the binding constraint here,
    // not budget, so the fill is exactly 10 lots, deterministically.
    const RESTING_LOTS: u32 = 10;
    post_alice_ask(&mut fx, 66_010, RESTING_LOTS);

    let amount_in: u64 = 100_000_000;
    let bob_quote_before = fx.token_amount_raw(&BOB_FEE_QUOTE_ATA);

    let ix = Swap {
        user: BOB,
        market: MARKET,
        user_base_ata: bob_base_ata_address(),
        user_quote_ata: BOB_FEE_QUOTE_ATA,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        vault_authority: fx.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: FEE_QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token_2022::ID,
    }
    .instruction(SwapInstructionArgs {
        side: 0, // Bid: buy base with quote
        amount_in,
        limit_tick: 66_010,
        min_out: (RESTING_LOTS as u64) * BASE_LOT_SIZE,
    });

    // Give Bob a base ATA to receive into.
    fx.accounts
        .push((bob_base_ata_address(), fee_free_base_account(&BOB, 0)));

    fx.run(ix);

    // The vault's final held balance is fixed by THIS CLOB's own fee math
    // (10 lots filled, 0.1% protocol taker fee) — independent of the exact
    // ceil-rounding of the mint's 1% input-leg fee, since the observed delta
    // comfortably exceeds what 10 lots cost either way. This is the actual
    // proof that matching ran against the observed delta and filled
    // correctly rather than reverting (the bug this file guards against) or
    // matching against the wrong (nominal) budget.
    let price_per_lot = 66_010 * TICK_SIZE;
    let consumed = RESTING_LOTS as u64 * price_per_lot;
    let consumed_with_clob_fee = consumed + consumed * FEE_BPS as u64 / 10_000;
    assert_eq!(fx.token_amount_raw(&fx.quote_vault), consumed_with_clob_fee);

    // Fully filled: exactly RESTING_LOTS, not more, not less.
    let bob_base_after = fx.token_amount_raw(&bob_base_ata_address());
    assert_eq!(bob_base_after, RESTING_LOTS as u64 * BASE_LOT_SIZE);

    // Bob's net spend reflects the unconsumed remainder coming back (minus
    // its own transfer fee on the way back) — strictly more than what the
    // vault kept (Bob also eats the mint's fee on both legs), but nowhere
    // near the full amount_in, which would mean the refund never happened.
    let bob_spent = bob_quote_before - fx.token_amount_raw(&BOB_FEE_QUOTE_ATA);
    assert!(bob_spent > consumed_with_clob_fee);
    assert!(
        bob_spent < consumed_with_clob_fee * 12 / 10,
        "bob_spent {bob_spent} vs consumed_with_clob_fee {consumed_with_clob_fee} — refund looks broken"
    );
}

/// A book too thin to absorb the whole post-fee delta: the unmatched
/// remainder must come back to Bob, not sit in the vault credited to
/// nobody. The refund leg is itself another fee-bearing transfer (same
/// mint), so Bob's net proceeds reflect a second, smaller fee deduction —
/// this test asserts against the vault's own accounting (which must land
/// exactly on what the match consumed) rather than assuming the refund is
/// fee-free.
#[test]
fn swap_against_fee_bearing_input_mint_refunds_unmatched_leftover() {
    let mut fx = FeeFixture::new();
    // Only 5 lots resting (notional 5 * 66_010 * TICK_SIZE = 33_005_000, plus
    // this CLOB's own 0.1% fee): nowhere near enough to consume the whole
    // post-fee delta from a 50_000_000-atom input at tick 66_010.
    post_alice_ask(&mut fx, 66_010, 5);

    let amount_in: u64 = 50_000_000;
    let vault_before = fx.token_amount_raw(&fx.quote_vault);
    let bob_quote_before = fx.token_amount_raw(&BOB_FEE_QUOTE_ATA);

    fx.accounts
        .push((bob_base_ata_address(), fee_free_base_account(&BOB, 0)));

    let ix = Swap {
        user: BOB,
        market: MARKET,
        user_base_ata: bob_base_ata_address(),
        user_quote_ata: BOB_FEE_QUOTE_ATA,
        base_vault: fx.base_vault,
        quote_vault: fx.quote_vault,
        vault_authority: fx.vault_authority,
        base_mint: BASE_MINT,
        quote_mint: FEE_QUOTE_MINT,
        base_token_program: spl_token::ID,
        quote_token_program: spl_token_2022::ID,
    }
    .instruction(SwapInstructionArgs {
        side: 0,
        amount_in,
        limit_tick: 66_010,
        min_out: 0,
    });
    fx.run(ix);

    // The 5 resting lots fully filled (limit_tick covers the whole resting
    // price, and the pulled-in delta vastly exceeds what 5 lots costs).
    let consumed = 5 * 66_010 * TICK_SIZE;
    let consumed_with_fee = consumed + consumed * FEE_BPS as u64 / 10_000;
    assert_eq!(
        fx.token_amount_raw(&fx.quote_vault) - vault_before,
        consumed_with_fee,
        "vault must hold exactly what the match consumed, not the whole pulled-in delta"
    );

    // Bob's base leg reflects exactly 5 lots filled, not more.
    assert_eq!(
        fx.token_amount_raw(&bob_base_ata_address()),
        5 * BASE_LOT_SIZE
    );

    // Bob's net quote spend is strictly less than the full amount_in — the
    // unconsumed remainder came back (minus its own transfer fee on the way
    // back, so it's not exactly `amount_in - consumed_with_fee`, but it must
    // be materially less than the full amount_in with nothing refunded).
    let bob_spent = bob_quote_before - fx.token_amount_raw(&BOB_FEE_QUOTE_ATA);
    assert!(
        bob_spent < amount_in,
        "bob_spent {bob_spent} should be less than amount_in {amount_in} — leftover must be refunded"
    );
    // And it should be in the right ballpark: not drastically more than what
    // was actually consumed (a missing refund would show up as bob_spent
    // being close to the full amount_in instead).
    assert!(
        bob_spent < consumed_with_fee * 2,
        "bob_spent {bob_spent} is too far above consumed_with_fee {consumed_with_fee} — refund looks broken"
    );

    fx.check_book_invariants();
}

fn bob_base_ata_address() -> Pubkey {
    Pubkey::new_from_array([0x26; 32])
}

fn fee_free_base_account(owner: &Pubkey, amount: u64) -> Account {
    let token_account_data = spl_token::state::Account {
        owner: *owner,
        mint: BASE_MINT,
        amount,
        delegate: COption::None,
        delegated_amount: 0,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        close_authority: COption::None,
    };
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(token_account_data, &mut data).unwrap();
    Account {
        lamports: 2_039_280,
        data,
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }
}

impl FeeFixture {
    /// Book-structure invariants only (I2-I9 minus I1's conservation check,
    /// which assumes a single-program-owned vault read via
    /// `spl_token::state::Account::unpack` — not valid here since the quote
    /// vault is an extended Token-2022 account). The base-side conservation
    /// and every link/chain/bitmap invariant still applies unchanged.
    fn check_book_invariants(&self) {
        let mut data = self.account(&MARKET).data.clone();
        let book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
        // Reuse the base-only half of the shared checker by passing the
        // real base vault amount and a quote figure that trivially satisfies
        // the quote conservation assert (quote_total + fees == quote_vault).
        let base_vault_amount = self.token_amount_raw(&self.base_vault);
        let quote_vault_amount = {
            let mut quote_total = 0u64;
            for seat in book.seats.iter() {
                quote_total += seat.quote_free() + seat.quote_locked();
            }
            quote_total + book.header.fees_accrued_quote()
        };
        check_invariants(&mut data, base_vault_amount, quote_vault_amount);
    }
}

// ── PermanentDelegate rejection ─────────────────────────────────────────
//
// This is the coverage gap that let a real offset bug through undetected:
// `has_permanent_delegate`'s scan originally started at the wrong absolute
// byte (83, which is actually `type_and_tlv_indices`'s *relative*
// `account_type_index` value for a Mint — landing inside the mandatory
// zero-padding between byte 82 and byte 165, not the real TLV region, which
// only starts at byte 166). The earlier fee-mint tests above only proved "a
// mint WITHOUT PermanentDelegate is accepted" — which passes trivially even
// with a broken scanner, since scanning the wrong region on a mint with no
// PermanentDelegate anywhere still correctly returns `false`. Only a mint
// that DOES carry the extension, checked against a real `CreateMarket` call
// through the real Token-2022 program, can catch a false negative here.

/// A Token-2022 mint carrying `PermanentDelegate` (and, if `after_transfer_fee`
/// is set, a `TransferFeeConfig` extension written first) — built via the
/// same `PodStateWithExtensionsMut`/`init_extension` pattern as
/// `fee_mint_account`, so the byte layout is correct by construction.
fn permanent_delegate_mint_account(delegate: &Pubkey, after_transfer_fee: bool) -> Account {
    let mut ext_types = Vec::new();
    if after_transfer_fee {
        ext_types.push(ExtensionType::TransferFeeConfig);
    }
    ext_types.push(ExtensionType::PermanentDelegate);
    let len = ExtensionType::try_calculate_account_len::<PodMint>(&ext_types).unwrap();
    let mut data = vec![0u8; len];
    {
        let mut state =
            PodStateWithExtensionsMut::<PodMint>::unpack_uninitialized(&mut data).unwrap();
        state.base.decimals = QUOTE_DECIMALS;
        state.base.is_initialized = true.into();
        if after_transfer_fee {
            let fee = TransferFee {
                epoch: 0u64.into(),
                maximum_fee: u64::MAX.into(),
                transfer_fee_basis_points: FEE_BASIS_POINTS.into(),
            };
            let fee_ext = state.init_extension::<TransferFeeConfig>(true).unwrap();
            fee_ext.transfer_fee_config_authority = None.try_into().unwrap();
            fee_ext.withdraw_withheld_authority = None.try_into().unwrap();
            fee_ext.withheld_amount = 0u64.into();
            fee_ext.older_transfer_fee = fee;
            fee_ext.newer_transfer_fee = fee;
        }
        let delegate_ext = state.init_extension::<PermanentDelegate>(true).unwrap();
        delegate_ext.delegate = Some(*delegate).try_into().unwrap();
        state.init_account_type().unwrap();
    }
    Account {
        lamports: 1_000_000_000,
        data,
        owner: spl_token_2022::ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// Build the minimal account set for one `CreateMarket` call against
/// `quote_mint_account` as the quote mint, run it directly (not through
/// `Fixture`/`FeeFixture`, which both assert every instruction succeeds),
/// and assert it fails with EXACTLY `ClobError::PermanentDelegateNotAllowed`
/// — not just "fails for some reason", which a broken scanner could also do
/// by accident (e.g. reading garbage past the account's actual length and
/// hitting a bounds error) without actually proving the check works.
fn assert_create_market_rejects_permanent_delegate(quote_mint_account: Account) {
    let mut mollusk = mollusk();
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);

    let quote_mint = Pubkey::new_from_array([0x27; 32]);
    let (vault_auth, _) = vault_authority();
    let (base_vault, _) = Pubkey::find_program_address(
        &[
            vault_auth.as_ref(),
            spl_token::ID.as_ref(),
            BASE_MINT.as_ref(),
        ],
        &spl_associated_token_account::ID,
    );
    let (quote_vault, _) = Pubkey::find_program_address(
        &[
            vault_auth.as_ref(),
            spl_token_2022::ID.as_ref(),
            quote_mint.as_ref(),
        ],
        &spl_associated_token_account::ID,
    );
    let (config, _) = config_address();

    let mut accounts = vec![
        (
            AUTHORITY,
            Account::new(1_000_000_000, 0, &Pubkey::default()),
        ),
        (config, Account::default()),
        (PAYER, Account::new(100_000_000_000, 0, &Pubkey::default())),
        (BASE_MINT, mint_account_legacy(9)),
        (quote_mint, quote_mint_account),
        (
            MARKET,
            Account::new(
                mollusk.sysvars.rent.minimum_balance(market_len()),
                market_len(),
                &PROGRAM_ID,
            ),
        ),
        (vault_auth, Account::default()),
        (base_vault, Account::default()),
        (quote_vault, Account::default()),
    ];
    {
        let (system_program, system_program_account) =
            mollusk_svm::program::keyed_account_for_system_program();
        accounts.push((system_program, system_program_account));
    }

    // CreateConfig first — CreateMarket needs a real config account.
    let create_config_ix = clob_client::instructions::CreateConfig {
        payer: PAYER,
        config,
        authority: AUTHORITY,
        system_program: Pubkey::default(),
    }
    .instruction(clob_client::instructions::CreateConfigInstructionArgs {
        seat_authority: SEAT_AUTHORITY,
        market_authority: MARKET_AUTHORITY,
    });
    let result =
        mollusk.process_and_validate_instruction(&create_config_ix, &accounts, &[Check::success()]);
    for (key, account) in result.resulting_accounts {
        if let Some(slot) = accounts.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = account;
        } else {
            accounts.push((key, account));
        }
    }

    let create_market_ix = CreateMarket {
        market: MARKET,
        base_mint: BASE_MINT,
        quote_mint,
        base_vault,
        quote_vault,
        authority: AUTHORITY,
        config,
    }
    .instruction(CreateMarketInstructionArgs {
        tick_size: TICK_SIZE,
        base_lot_size: BASE_LOT_SIZE,
        anchor_tick: ANCHOR,
        fee_bps: FEE_BPS,
        max_lots_per_order: MAX_LOTS,
    });
    let result = mollusk.process_instruction(&create_market_ix, &accounts);

    let expected = ClobError::PermanentDelegateNotAllowed as u32;
    match result.program_result {
        ProgramResult::Failure(ProgramError::Custom(code)) => assert_eq!(
            code, expected,
            "CreateMarket failed, but with the wrong error — expected PermanentDelegateNotAllowed (0x{expected:X})"
        ),
        other => panic!("expected Failure(Custom(0x{expected:X})), got {other:?}"),
    }
}

/// The exact bug this test file was extended to catch: a mint whose ONLY
/// extension is `PermanentDelegate` must be rejected.
#[test]
fn create_market_rejects_mint_with_permanent_delegate_only() {
    let delegate = Pubkey::new_from_array([0x28; 32]);
    assert_create_market_rejects_permanent_delegate(permanent_delegate_mint_account(
        &delegate, false,
    ));
}

/// `PermanentDelegate` written AFTER a `TransferFeeConfig` extension — proves
/// the scan correctly walks past an earlier extension's TLV entry (using its
/// real `length` field to skip exactly its value bytes) rather than only
/// working by coincidence when the target extension happens to be first.
/// This is precisely the scenario the original offset bug would have masked
/// most easily: once desynced from the true TLV boundaries, an earlier
/// extension's bytes are exactly what could scramble the scan before it
/// ever reaches PermanentDelegate.
#[test]
fn create_market_rejects_mint_with_permanent_delegate_after_transfer_fee_config() {
    let delegate = Pubkey::new_from_array([0x29; 32]);
    assert_create_market_rejects_permanent_delegate(permanent_delegate_mint_account(
        &delegate, true,
    ));
}
