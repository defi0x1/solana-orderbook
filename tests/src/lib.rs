//! Shared fixtures for the clob integration tests and CU benches.
//!
//! Everything goes through the Codama-generated client (clob-client): the
//! builders are exercised against the real program, so any drift between the
//! IDL tree and the program wire format fails here.

use clob_program::test_utils::*;
use mollusk_svm::result::Check;
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_token::solana_program::{program_option::COption, program_pack::Pack};

pub const PROGRAM_ID: Pubkey = Pubkey::new_from_array(clob_program::ID);
pub const AUTHORITY: Pubkey = Pubkey::new_from_array([0xaa; 32]);
pub const ALICE: Pubkey = Pubkey::new_from_array([0xa1; 32]); // maker
pub const BOB: Pubkey = Pubkey::new_from_array([0xb0; 32]); // taker
pub const PAYER: Pubkey = Pubkey::new_from_array([0xcc; 32]);
pub const BASE_MINT: Pubkey = Pubkey::new_from_array([0xdd; 32]);
pub const QUOTE_MINT: Pubkey = Pubkey::new_from_array([0xee; 32]);
pub const MARKET: Pubkey = Pubkey::new_from_array([0x11; 32]);
/// Venue roles. One key plays all three in tests.
pub const SEAT_AUTHORITY: Pubkey = AUTHORITY;
pub const MARKET_AUTHORITY: Pubkey = AUTHORITY;

// Market parameters used across all tests.
pub const TICK_SIZE: u64 = 100; // quote atoms per base lot per tick
pub const BASE_LOT_SIZE: u64 = 1_000; // base atoms per lot
pub const ANCHOR: u32 = 64_000;
pub const FEE_BPS: u16 = 10; // 0.1%
pub const MAX_LOTS: u32 = 1_000_000;
pub const CAPACITY: usize = 4_096;

pub fn market_len() -> usize {
    POOL_OFFSET + CAPACITY * NODE_LEN
}

pub fn mollusk() -> Mollusk {
    let mut m = Mollusk::new(&PROGRAM_ID, "../target/deploy/clob_program");
    mollusk_svm_programs_token::token::add_program(&mut m);
    m
}

pub fn vault_authority() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED, MARKET.as_ref()], &PROGRAM_ID)
}

pub fn config_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CONFIG_SEED, AUTHORITY.as_ref()], &PROGRAM_ID)
}

/// Grant a seat to `owner`, co-signed by the venue's seat authority.
pub fn claim_seat_ix(owner: Pubkey) -> Instruction {
    clob_client::instructions::ClaimSeat {
        seat_authority: SEAT_AUTHORITY,
        owner,
        config: config_address().0,
        market: MARKET,
    }
    .instruction()
}

/// Move the price window, co-signed by the venue's market authority.
pub fn reanchor_ix(target_anchor: u32, max_levels: u32) -> Instruction {
    clob_client::instructions::Reanchor {
        market_authority: MARKET_AUTHORITY,
        config: config_address().0,
        market: MARKET,
    }
    .instruction(clob_client::instructions::ReanchorInstructionArgs {
        target_anchor,
        max_levels,
    })
}

/// A real, initialized SPL Mint. Needed since `TransferChecked` (unlike the
/// legacy unchecked `Transfer` this fixture predates) has the token program
/// unpack the mint account itself — a zero-filled placeholder has
/// `is_initialized = false` and the token program rejects that with
/// `UninitializedAccount` before it ever gets to moving atoms.
fn mint_account(decimals: u8) -> Account {
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

pub fn token_account(owner: &Pubkey, mint: &Pubkey, amount: u64) -> (Pubkey, Account) {
    let token_account_data = spl_token::state::Account {
        owner: *owner,
        mint: *mint,
        amount,
        delegate: COption::None,
        delegated_amount: 0,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        close_authority: COption::None,
    };

    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(token_account_data, &mut data).unwrap();

    let address =
        mollusk_svm_programs_token::associated_token::create_account_for_associated_token_account(
            token_account_data,
        )
        .0;

    (
        address,
        Account {
            lamports: 2_039_280,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// A living account set threaded through a sequence of instructions.
pub struct Fixture {
    pub mollusk: Mollusk,
    pub accounts: Vec<(Pubkey, Account)>,
    pub vault_authority: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub alice_base_ata: Pubkey,
    pub alice_quote_ata: Pubkey,
    pub bob_base_ata: Pubkey,
    pub bob_quote_ata: Pubkey,
}

impl Fixture {
    /// Build all accounts and run CreateMarket.
    pub fn new() -> Self {
        let mollusk = mollusk();
        let (vault_auth, _) = vault_authority();

        let (base_vault, base_vault_account) = token_account(&vault_auth, &BASE_MINT, 0);
        let (quote_vault, quote_vault_account) = token_account(&vault_auth, &QUOTE_MINT, 0);
        let (alice_base_ata, alice_base) = token_account(&ALICE, &BASE_MINT, 1_000_000_000);
        let (alice_quote_ata, alice_quote) = token_account(&ALICE, &QUOTE_MINT, 100_000_000_000);
        let (bob_base_ata, bob_base) = token_account(&BOB, &BASE_MINT, 1_000_000_000);
        let (bob_quote_ata, bob_quote) = token_account(&BOB, &QUOTE_MINT, 100_000_000_000);
        let (system_program, system_program_account) =
            mollusk_svm::program::keyed_account_for_system_program();
        let (token_program, token_program_account) =
            mollusk_svm_programs_token::token::keyed_account();

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
            (BASE_MINT, mint_account(9)),
            (QUOTE_MINT, mint_account(6)),
            (MARKET, market_account),
            (vault_auth, Account::default()),
            (base_vault, base_vault_account),
            (quote_vault, quote_vault_account),
            (alice_base_ata, alice_base),
            (alice_quote_ata, alice_quote),
            (bob_base_ata, bob_base),
            (bob_quote_ata, bob_quote),
            (system_program, system_program_account),
            (token_program, token_program_account),
        ];

        let mut fixture = Self {
            mollusk,
            accounts,
            vault_authority: vault_auth,
            base_vault,
            quote_vault,
            alice_base_ata,
            alice_quote_ata,
            bob_base_ata,
            bob_quote_ata,
        };

        let ix = clob_client::instructions::CreateConfig {
            payer: PAYER,
            config,
            authority: AUTHORITY,
            system_program: solana_pubkey::Pubkey::default(),
        }
        .instruction(clob_client::instructions::CreateConfigInstructionArgs {
            seat_authority: SEAT_AUTHORITY,
            market_authority: MARKET_AUTHORITY,
        });
        fixture.run(ix);

        let ix = clob_client::instructions::CreateMarket {
            market: MARKET,
            base_mint: BASE_MINT,
            quote_mint: QUOTE_MINT,
            base_vault,
            quote_vault,
            authority: AUTHORITY,
            config,
        }
        .instruction(clob_client::instructions::CreateMarketInstructionArgs {
            tick_size: TICK_SIZE,
            base_lot_size: BASE_LOT_SIZE,
            anchor_tick: ANCHOR,
            fee_bps: FEE_BPS,
            max_lots_per_order: MAX_LOTS,
        });
        fixture.run(ix);
        fixture
    }

    /// Execute one instruction, asserting success and folding the resulting
    /// accounts back into the living set.
    pub fn run(&mut self, ix: Instruction) {
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

    /// Execute one instruction, asserting it fails; state is not merged.
    pub fn run_expect_failure(&mut self, ix: Instruction) {
        let result = self.mollusk.process_instruction(&ix, &self.accounts);
        assert!(
            result.program_result.is_err(),
            "instruction unexpectedly succeeded"
        );
    }

    pub fn account(&self, key: &Pubkey) -> &Account {
        &self.accounts.iter().find(|(k, _)| k == key).unwrap().1
    }

    pub fn market_data(&self) -> Vec<u8> {
        self.account(&MARKET).data.clone()
    }

    pub fn token_amount(&self, key: &Pubkey) -> u64 {
        spl_token::state::Account::unpack(&self.account(key).data)
            .unwrap()
            .amount
    }

    /// Check the standing invariants against the current market state.
    pub fn check_invariants(&self) {
        let mut data = self.market_data();
        check_invariants(
            &mut data,
            self.token_amount(&self.base_vault),
            self.token_amount(&self.quote_vault),
        );
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Standing invariants (the design's I1–I9), checked against raw market bytes.
pub fn check_invariants(data: &mut [u8], base_vault_amount: u64, quote_vault_amount: u64) {
    let book = unsafe { BookRefMut::from_bytes_unchecked_mut(data) };
    let anchor = book.header.anchor_tick();
    let capacity = book.header.order_pool_capacity() as usize;

    // I2: best_bid < best_ask.
    if let (Some(bb), Some(ba)) = (book.best_bid(), book.best_ask()) {
        assert!(bb < ba, "crossed book: best_bid {bb} >= best_ask {ba}");
    }

    let mut node_state = vec![0u8; capacity]; // 0 unseen, 1 live, 2 free, 4 live+chained

    // I3 + I4: bitmap consistency and level totals.
    for offset in 0..WINDOW_TICKS as u32 {
        let tick = anchor + offset;
        let bid = book.bid_bitmap.get(tick);
        let ask = book.ask_bitmap.get(tick);
        assert!(!(bid && ask), "tick {tick} set on both sides");
        let lvl = &book.levels[(tick & WINDOW_MASK) as usize];
        if bid || ask {
            let mut cur = lvl.head();
            let mut sum = 0u64;
            assert_ne!(cur, NIL, "occupied tick {tick} with empty level");
            while cur != NIL {
                let node = &book.pool[cur as usize];
                let expected_side = if bid { Side::Bid } else { Side::Ask };
                assert!(
                    node.lots() > 0,
                    "zero-lot node {cur} linked in level {tick}"
                );
                assert_ne!(
                    node.seq(),
                    FREED_SEQ,
                    "freed node {cur} linked in level {tick}"
                );
                assert_eq!(node.tick(), tick, "node {cur} tick mismatch");
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

    // Node 0 is the reserved NIL node: it may never appear on any list.
    assert_eq!(node_state[0], 0, "reserved node 0 linked in a level");

    // I6: free list disjoint from live nodes and correctly counted. Node 0 is
    // neither free nor live, so free + live == capacity - 1.
    let mut free = 0u32;
    let mut cur = book.header.free_head();
    while cur != NIL {
        assert_eq!(node_state[cur as usize], 0, "free node {cur} also live");
        assert_eq!(
            book.pool[cur as usize].seq(),
            FREED_SEQ,
            "free node {cur} without FREED_SEQ"
        );
        node_state[cur as usize] = 2;
        free += 1;
        assert!(free < capacity as u32, "free list cycle");
        cur = book.pool[cur as usize].next();
    }
    assert_eq!(free, book.header.free_count(), "free_count mismatch");

    // I7 + I8: seat chains partition the live nodes, sorted strictly by
    // (side, tick) — strict also enforces the slab invariant of at most one
    // order per (side, tick) per maker — with intact back-links and an exact
    // order_count; locked balances equal exactly what the orders require.
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
        while cur != NIL {
            let node = &book.pool[cur as usize];
            assert_eq!(node.maker() as usize, i, "node {cur} maker mismatch");
            assert_eq!(node.maker_prev(), prev, "node {cur} back-link broken");
            assert!(node.lots() > 0, "seat {i} chains node {cur} with no lots");
            assert_eq!(
                node_state[cur as usize], 1,
                "chained node {cur} not in a level"
            );
            node_state[cur as usize] = 4; // live and chained
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
                Side::Bid => {
                    quote_locked +=
                        node.tick() as u64 * book.header.tick_size() * node.lots() as u64
                }
                Side::Ask => base_locked += node.lots() as u64 * book.header.base_lot_size(),
            }
            prev = cur;
            cur = node.maker_next();
        }
        assert_eq!(count, seat.order_count(), "seat {i} order_count mismatch");
        assert_eq!(
            base_locked,
            seat.base_locked(),
            "seat {i} base lock mismatch"
        );
        assert_eq!(
            quote_locked,
            seat.quote_locked(),
            "seat {i} quote lock mismatch"
        );
    }

    // Every live node must be chained to some seat.
    for (idx, state) in node_state.iter().enumerate() {
        assert_ne!(*state, 1, "live node {idx} not in any seat chain");
    }

    // I1: conservation against the vaults.
    assert_eq!(base_total, base_vault_amount, "base conservation violated");
    assert_eq!(
        quote_total + book.header.fees_accrued_quote(),
        quote_vault_amount,
        "quote conservation violated"
    );
}

/// Convenience quote constructor for mass_quote payloads.
pub fn q(tick: u32, lots: u32) -> clob_client::types::Quote {
    clob_client::types::Quote {
        tick,
        lots,
        flags: 0,
    }
}
