//! The order book: state layout and every Execution Helper that mutates it.
//!
//! # One account, five regions
//!
//! A market is a single account laid out as: a fixed 320-byte [`Market`]
//! header, two occupancy [`Bitmap`]s (bids, asks), 8,192 [`Level`]s, 128
//! [`Seat`]s, and a growable pool of [`OrderNode`]s (region offsets in
//! `constants.rs`). Every struct here has alignment 1 — all fields are
//! little-endian byte arrays decoded on access — so the regions can be
//! overlaid on raw account bytes at any offset with no alignment hazard, on
//! chain and off.
//!
//! # Prices are ticks; the book is a ring
//!
//! A price is an absolute tick (`price = tick * tick_size`). The book covers
//! a moving window of 8,192 consecutive ticks starting at `anchor_tick`, and
//! both the level array and the bitmaps are indexed by `tick & WINDOW_MASK` —
//! a ring over tick space. Recentering the window therefore moves NO memory:
//! it re-labels which physical slots are valid and evicts the vacated edge.
//!
//! # Zeroed means empty; the bitmap is the occupancy index
//!
//! `NIL == 0` and node 0 is reserved at creation (never allocated), so an
//! all-zero region *is* the empty state: an empty level's head, an empty seat
//! chain and an empty free list all read as NIL with no interpretation rule
//! attached. The bitmaps exist for speed (best price = a couple of
//! count-zeros instructions), and they agree with the levels by invariant
//! I4 — not because readers must consult them first to stay safe. The bid and
//! ask bitmaps are disjoint by the crossing invariant (`best_bid <
//! best_ask`), which is also what lets bids and asks share one level array: a
//! tick belongs to at most one side.
//!
//! # Node lifecycle: live or free — nothing in between
//!
//! A LIVE node rests on a level (FIFO links `prev`/`next`) and on its maker's
//! seat chain (`maker_prev`/`maker_next`, sorted bids-then-asks by tick). The
//! chain is doubly linked so removal is O(1) from any context: the moment an
//! order fills, cancels, expires or is evicted, it is unlinked from both
//! lists and FREEd in one step. There is no dead state, no deferred sweep,
//! and `Seat::order_count` counts exactly the live orders. FREE nodes carry
//! `FREED_SEQ` (unreachable for live ids) and
//! thread the free list through `next`.
//!
//! # Seats are the settlement layer
//!
//! Funds live in per-seat internal balances (atoms), locked when an order
//! posts and released on cancel/evict or settled on fill. Every fill is plain
//! arithmetic on two seats — no token movement, no CPI. Credits use wrapping
//! adds (bounded by real deposits, which cannot exceed a mint's u64 supply);
//! debits of locked balances are CHECKED: a refund larger than the lock is
//! corruption, and the instruction aborts rather than minting the difference.
//!
//! # Why nothing here touches `AccountInfo`
//!
//! Every helper takes and returns plain values over the raw bytes, and the
//! current slot arrives as a `u64` argument. That keeps the matching logic
//! runnable off-chain against a copy of the account — in tests, in analysis
//! tools, and in anything that needs to answer "what would this order do"
//! without simulating a transaction.
//!
//! # Arithmetic
//!
//! All value math is u64. `create_market` rejects parameter sets where
//! `tick_limit * tick_size * max_lots_per_order * (10_000 + max fee)` could
//! overflow, and every order is bounded by `tick_limit` and
//! `max_lots_per_order` — so the implicit arithmetic in this file is proven
//! overflow-free at market creation, and release builds compile without
//! overflow checks. Deliberate guards use explicit `checked_*`; deliberate
//! modular ops use explicit `wrapping_*`.

use crate::constants::*;
use crate::errors::ClobError;
use core::mem::size_of;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey};

/// Branch-layout hints — pinocchio 0.8 has no `hint` module, so this is the
/// classic cold-marker trick. `likely(c)` lays the true arm out as the
/// fall-through; conditions wrapped in `unlikely` sink their arm (error
/// construction and all) out of the hot instruction sequence.
#[cold]
#[inline(never)]
fn cold_marker() {}

#[inline(always)]
pub fn likely(b: bool) -> bool {
    if !b {
        cold_marker();
    }
    b
}

#[inline(always)]
pub fn unlikely(b: bool) -> bool {
    if b {
        cold_marker();
    }
    b
}

/// Order side. Bids sort before asks in a seat's order chain.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    #[inline(always)]
    pub fn try_from_u8(v: u8) -> Result<Self, ProgramError> {
        match v {
            0 => Ok(Side::Bid),
            1 => Ok(Side::Ask),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }

    #[inline(always)]
    pub fn opposite(self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Self-trade policy for taker instructions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelfTradePolicy {
    /// Cancel the resting order (refund the maker — the taker itself) and keep matching.
    CancelResting,
    /// Abort the whole instruction.
    Abort,
}

impl SelfTradePolicy {
    #[inline(always)]
    pub fn from_flags(flags: u8) -> Self {
        if flags & 0b0000_0010 != 0 {
            SelfTradePolicy::Abort
        } else {
            SelfTradePolicy::CancelResting
        }
    }
}

/// Result of a matching pass.
#[derive(Clone, Copy, Default)]
pub struct TakeResult {
    /// Base lots filled.
    pub lots: u32,
    /// Quote atoms traded (maker-side notional, fee excluded).
    pub quote: u64,
    /// Fee in quote atoms, accrued to the market.
    pub fee: u64,
}

pub(crate) struct TakeParams {
    pub(crate) side: Side,
    pub(crate) limit_tick: u32,
    pub(crate) max_lots: u32,
    pub(crate) max_quote: u64,
    pub(crate) seat: u16,
    pub(crate) slot: u64,
    pub(crate) self_trade: SelfTradePolicy,
}

/// # Market
///
/// Header of the single per-market account. Fixed 320 bytes at offset 0,
/// followed by the bid/ask bitmaps, the level array, the seat table, and the
/// growable order pool (offsets in constants.rs).
///
/// All fields are little-endian byte arrays: the struct has alignment 1, so it
/// can be overlaid at any offset of the account data with no alignment hazard.
#[repr(C)]
pub struct Market {
    version: [u8; 1],
    bump: [u8; 1],
    base_mint: [u8; 32],
    quote_mint: [u8; 32],
    base_vault: [u8; 32],
    quote_vault: [u8; 32],
    vault_authority: [u8; 32],
    authority: [u8; 32],
    config: [u8; 32],
    tick_size: [u8; 8],     // quote atoms per base lot per tick
    base_lot_size: [u8; 8], // base atoms per lot
    anchor_tick: [u8; 4],   // window start, multiple of 64
    tick_limit: [u8; 4],    // highest tick where u64 math stays safe
    fee_bps: [u8; 2],
    max_lots_per_order: [u8; 4],
    order_pool_capacity: [u8; 4],
    free_head: [u8; 2],
    free_count: [u8; 4],
    seq: [u8; 16],      // global op counter (+1 per mutating ix)
    order_seq: [u8; 8], // next 64-bit order id; all-ones is skipped
    fees_accrued_quote: [u8; 8],
    // Which SPL token program owns each mint (TOKEN_PROGRAM_LEGACY /
    // TOKEN_PROGRAM_2022) and that mint's decimals, recorded once at
    // CreateMarket so Deposit/Withdraw/Swap never re-derive or re-parse the
    // mint account to route a transfer or build TransferChecked's decimals
    // field.
    base_token_program: [u8; 1],
    quote_token_program: [u8; 1],
    base_decimals: [u8; 1],
    quote_decimals: [u8; 1],
    _padding: [u8; 18],
}

impl Market {
    pub const LEN: usize = 320;

    /* Reading Helpers */

    /// Validate a market account: owner, minimum length, version, and that the
    /// account length matches the pool capacity recorded in the header.
    ///
    /// This is the ONLY validation hot-path instructions perform on the market
    /// account. No PDA derivation, ever.
    #[inline(always)]
    pub fn check(account_info: &AccountInfo) -> Result<(), ProgramError> {
        // Check if the owner is the expected owner
        if unlikely(!account_info.is_owned_by(&crate::ID)) {
            return Err(ClobError::InvalidAccountOwner.into());
        }

        // Check if the data length is at least the minimum length
        if unlikely(account_info.data_len() < MIN_MARKET_LEN) {
            return Err(ClobError::InvalidAccountLength.into());
        }

        let header = unsafe { Self::from_bytes_unchecked(account_info.borrow_data_unchecked()) };

        // Check the version
        if unlikely(header.version().ne(&MARKET_VERSION)) {
            return Err(ClobError::InvalidVersion.into());
        }

        // Check that the length matches the recorded pool capacity
        let expected = POOL_OFFSET + header.order_pool_capacity() as usize * NODE_LEN;
        if unlikely(account_info.data_len().ne(&expected)) {
            return Err(ClobError::InvalidAccountLength.into());
        }

        Ok(())
    }

    /// Return a `Market` from the given bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `bytes` contains a valid representation of
    /// the market header. Alignment is 1, so any offset is safe.
    #[inline(always)]
    pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> &Self {
        &*(bytes.as_ptr() as *const Market)
    }

    /// Return a mutable `Market` from the given bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `bytes` contains a valid representation of
    /// the market header and that no other references overlap it.
    #[inline(always)]
    pub unsafe fn from_bytes_unchecked_mut(bytes: &mut [u8]) -> &mut Self {
        &mut *(bytes.as_mut_ptr() as *mut Market)
    }

    #[inline(always)]
    pub fn version(&self) -> u8 {
        self.version[0]
    }

    #[inline(always)]
    pub fn bump(&self) -> &[u8; 1] {
        &self.bump
    }

    #[inline(always)]
    pub fn base_vault(&self) -> &[u8; 32] {
        &self.base_vault
    }

    #[inline(always)]
    pub fn quote_vault(&self) -> &[u8; 32] {
        &self.quote_vault
    }

    #[inline(always)]
    pub fn vault_authority(&self) -> &[u8; 32] {
        &self.vault_authority
    }

    #[inline(always)]
    pub fn authority(&self) -> &[u8; 32] {
        &self.authority
    }

    /// The config account that decides who may take a seat on this market and
    /// who may move its price window.
    #[inline(always)]
    pub fn config(&self) -> &[u8; 32] {
        &self.config
    }

    #[inline(always)]
    pub fn tick_size(&self) -> u64 {
        u64::from_le_bytes(self.tick_size)
    }

    #[inline(always)]
    pub fn base_lot_size(&self) -> u64 {
        u64::from_le_bytes(self.base_lot_size)
    }

    #[inline(always)]
    pub fn anchor_tick(&self) -> u32 {
        u32::from_le_bytes(self.anchor_tick)
    }

    #[inline(always)]
    pub fn tick_limit(&self) -> u32 {
        u32::from_le_bytes(self.tick_limit)
    }

    #[inline(always)]
    pub fn fee_bps(&self) -> u16 {
        u16::from_le_bytes(self.fee_bps)
    }

    #[inline(always)]
    pub fn max_lots_per_order(&self) -> u32 {
        u32::from_le_bytes(self.max_lots_per_order)
    }

    #[inline(always)]
    pub fn order_pool_capacity(&self) -> u32 {
        u32::from_le_bytes(self.order_pool_capacity)
    }

    #[inline(always)]
    pub fn free_head(&self) -> u16 {
        u16::from_le_bytes(self.free_head)
    }

    #[inline(always)]
    pub fn free_count(&self) -> u32 {
        u32::from_le_bytes(self.free_count)
    }

    /// Monotonic count of mutating instructions applied to this market. An
    /// indexer following account updates uses it to order snapshots and to
    /// detect that it missed one; paired with the market address it is a
    /// globally unique version for every state this account has ever held.
    #[cfg(feature = "test-utils")]
    #[inline(always)]
    pub fn fees_accrued_quote(&self) -> u64 {
        u64::from_le_bytes(self.fees_accrued_quote)
    }

    /// Quote atoms for `lots` at `tick`. Safe by the tick_limit invariant:
    /// tick <= tick_limit and lots <= max_lots_per_order imply no overflow.
    #[inline(always)]
    pub fn notional(&self, tick: u32, lots: u32) -> u64 {
        (tick as u64) * self.tick_size() * (lots as u64)
    }

    /// Base atoms for `lots`.
    #[inline(always)]
    pub fn base_atoms(&self, lots: u32) -> u64 {
        (lots as u64) * self.base_lot_size()
    }

    #[inline(always)]
    pub fn base_token_program(&self) -> u8 {
        self.base_token_program[0]
    }

    #[inline(always)]
    pub fn quote_token_program(&self) -> u8 {
        self.quote_token_program[0]
    }

    /// The program id each field resolves to (`TOKEN_PROGRAM_LEGACY` /
    /// `TOKEN_PROGRAM_2022`, as set by `CreateMarket` from the mint's actual
    /// owner).
    #[inline(always)]
    pub fn base_token_program_id(&self) -> Pubkey {
        crate::constants::token_program_id(self.base_token_program())
    }

    #[inline(always)]
    pub fn quote_token_program_id(&self) -> Pubkey {
        crate::constants::token_program_id(self.quote_token_program())
    }

    #[inline(always)]
    pub fn base_decimals(&self) -> u8 {
        self.base_decimals[0]
    }

    #[inline(always)]
    pub fn quote_decimals(&self) -> u8 {
        self.quote_decimals[0]
    }

    /* Writing Helpers */

    #[inline(always)]
    pub fn set_version(&mut self, version: u8) {
        self.version[0] = version;
    }

    #[inline(always)]
    pub fn set_bump(&mut self, bump: [u8; 1]) {
        self.bump = bump;
    }

    #[inline(always)]
    pub fn set_base_mint(&mut self, v: Pubkey) {
        self.base_mint = v;
    }

    #[inline(always)]
    pub fn set_quote_mint(&mut self, v: Pubkey) {
        self.quote_mint = v;
    }

    #[inline(always)]
    pub fn set_base_vault(&mut self, v: Pubkey) {
        self.base_vault = v;
    }

    #[inline(always)]
    pub fn set_quote_vault(&mut self, v: Pubkey) {
        self.quote_vault = v;
    }

    #[inline(always)]
    pub fn set_vault_authority(&mut self, v: Pubkey) {
        self.vault_authority = v;
    }

    #[inline(always)]
    pub fn set_authority(&mut self, v: Pubkey) {
        self.authority = v;
    }

    #[inline(always)]
    pub fn set_config(&mut self, v: Pubkey) {
        self.config = v;
    }

    #[inline(always)]
    pub fn set_tick_size(&mut self, v: u64) {
        self.tick_size = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_base_lot_size(&mut self, v: u64) {
        self.base_lot_size = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_anchor_tick(&mut self, v: u32) {
        self.anchor_tick = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_tick_limit(&mut self, v: u32) {
        self.tick_limit = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_fee_bps(&mut self, v: u16) -> Result<(), ProgramError> {
        if v.gt(&MAX_FEE_BPS) {
            return Err(ClobError::InvalidFee.into());
        }
        self.fee_bps = v.to_le_bytes();
        Ok(())
    }

    #[inline(always)]
    pub fn set_max_lots_per_order(&mut self, v: u32) {
        self.max_lots_per_order = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_base_token_program(&mut self, v: u8) {
        self.base_token_program = [v];
    }

    #[inline(always)]
    pub fn set_quote_token_program(&mut self, v: u8) {
        self.quote_token_program = [v];
    }

    #[inline(always)]
    pub fn set_base_decimals(&mut self, v: u8) {
        self.base_decimals = [v];
    }

    #[inline(always)]
    pub fn set_quote_decimals(&mut self, v: u8) {
        self.quote_decimals = [v];
    }

    #[inline(always)]
    pub fn set_order_pool_capacity(&mut self, v: u32) {
        self.order_pool_capacity = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_free_head(&mut self, v: u16) {
        self.free_head = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_free_count(&mut self, v: u32) {
        self.free_count = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn bump_seq(&mut self) {
        self.seq = (u128::from_le_bytes(self.seq).wrapping_add(1)).to_le_bytes();
    }

    /// Return the next order id and advance the counter. The all-ones value is
    /// skipped so a live node can never carry `FREED_SEQ`: the node seq alone
    /// distinguishes live from freed, with no separate flag.
    #[inline(always)]
    pub fn next_order_id(&mut self) -> u64 {
        let raw = u64::from_le_bytes(self.order_seq);
        let id = if raw == FREED_SEQ { 0 } else { raw };
        self.order_seq = id.wrapping_add(1).to_le_bytes();
        id
    }

    #[cfg(feature = "test-utils")]
    pub fn set_next_order_id_for_test(&mut self, id: u64) {
        self.order_seq = id.to_le_bytes();
    }

    #[inline(always)]
    pub fn add_fees_accrued(&mut self, fee: u64) {
        self.fees_accrued_quote =
            (u64::from_le_bytes(self.fees_accrued_quote).wrapping_add(fee)).to_le_bytes();
    }

    #[inline(always)]
    pub fn take_fees_accrued(&mut self) -> u64 {
        let v = u64::from_le_bytes(self.fees_accrued_quote);
        self.fees_accrued_quote = 0u64.to_le_bytes();
        v
    }
}

/// # Config
///
/// Venue-level permissions, shared by every market that names it. A market
/// records which config governs it at creation, so one operator can run many
/// markets under one set of keys, and changing those keys does not touch — or
/// write-lock — the market accounts themselves.
///
/// The two roles are separate on purpose: granting seats is routine onboarding
/// and gets delegated, while moving a market's price window is a rare
/// operational decision that relocates the tradeable price range.
#[repr(C)]
pub struct Config {
    version: [u8; 1],
    bump: [u8; 1],
    authority: [u8; 32],
    seat_authority: [u8; 32],
    market_authority: [u8; 32],
    _padding: [u8; 30],
}

impl Config {
    pub const LEN: usize = CONFIG_LEN;

    /// Validate a config account: owner, length and version.
    #[inline(always)]
    pub fn check(account_info: &AccountInfo) -> Result<(), ProgramError> {
        if !account_info.is_owned_by(&crate::ID) {
            return Err(ClobError::InvalidAccountOwner.into());
        }
        if account_info.data_len() != Self::LEN {
            return Err(ClobError::InvalidAccountLength.into());
        }
        let cfg = unsafe { Self::from_bytes_unchecked(account_info.borrow_data_unchecked()) };
        if cfg.version().ne(&CONFIG_VERSION) {
            return Err(ClobError::InvalidVersion.into());
        }
        Ok(())
    }

    /// # Safety
    ///
    /// `bytes` must hold a config account's data. Alignment is 1.
    #[inline(always)]
    pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> &Self {
        &*(bytes.as_ptr() as *const Config)
    }

    /// # Safety
    ///
    /// `bytes` must hold a config account's data and be uniquely borrowed.
    #[inline(always)]
    pub unsafe fn from_bytes_unchecked_mut(bytes: &mut [u8]) -> &mut Self {
        &mut *(bytes.as_mut_ptr() as *mut Config)
    }

    #[inline(always)]
    pub fn version(&self) -> u8 {
        self.version[0]
    }

    #[inline(always)]
    pub fn authority(&self) -> &[u8; 32] {
        &self.authority
    }

    /// May sign `ClaimSeat`.
    #[inline(always)]
    pub fn seat_authority(&self) -> &[u8; 32] {
        &self.seat_authority
    }

    /// May sign `Reanchor`.
    #[inline(always)]
    pub fn market_authority(&self) -> &[u8; 32] {
        &self.market_authority
    }

    #[inline(always)]
    pub fn set_version(&mut self, v: u8) {
        self.version[0] = v;
    }

    #[inline(always)]
    pub fn set_bump(&mut self, v: [u8; 1]) {
        self.bump = v;
    }

    #[inline(always)]
    pub fn set_authority(&mut self, v: Pubkey) {
        self.authority = v;
    }

    #[inline(always)]
    pub fn set_seat_authority(&mut self, v: Pubkey) {
        self.seat_authority = v;
    }

    #[inline(always)]
    pub fn set_market_authority(&mut self, v: Pubkey) {
        self.market_authority = v;
    }
}

/// # Bitmap
///
/// Two-level occupancy bitmap over the 8,192-tick ring window: 128 leaf words
/// of 64 bits (bit index for absolute tick `t` is `t & WINDOW_MASK`) plus a
/// 2-word summary in which bit `w` says "leaf word `w` is non-zero". Finding
/// the best price is a couple of count-zeros instructions: locate the first
/// interesting leaf word through the summary, then the first set bit in it.
///
/// The subtlety is that PHYSICAL bit order is not TICK order. The window
/// `[anchor, anchor + 8191]` maps onto physical positions modulo 8192, so the
/// lowest tick lives at position `anchor & WINDOW_MASK` — possibly in the
/// middle of the array — and tick order wraps around the physical end at most
/// once. Scans therefore run in up to two physical segments: from the start
/// position to the array end, then from the array start for the remainder.
/// `scan_up`/`scan_down` handle one physical (non-wrapping) segment;
/// `next_at_or_after`/`prev_at_or_before` split a tick-order query into the
/// two segments and translate positions back into absolute ticks.
#[repr(C)]
pub struct Bitmap {
    summary: [[u8; 8]; BITMAP_SUMMARY_WORDS],
    leaves: [[u8; 8]; BITMAP_LEAF_WORDS],
}

impl Bitmap {
    // Direct word-array indexing: with #[inline(always)] the index ranges
    // propagate from the (masked) tick math, so LLVM elides the bounds checks
    // and these compile to bare unaligned loads/stores.

    #[inline(always)]
    fn leaf(&self, w: usize) -> u64 {
        u64::from_le_bytes(self.leaves[w])
    }

    #[inline(always)]
    fn set_leaf(&mut self, w: usize, v: u64) {
        self.leaves[w] = v.to_le_bytes();
    }

    #[inline(always)]
    fn summary_word(&self, s: usize) -> u64 {
        u64::from_le_bytes(self.summary[s])
    }

    #[inline(always)]
    fn set_summary_word(&mut self, s: usize, v: u64) {
        self.summary[s] = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn get(&self, tick: u32) -> bool {
        let pos = (tick & WINDOW_MASK) as usize;
        self.leaf(pos >> 6) & (1u64 << (pos & 63)) != 0
    }

    #[inline(always)]
    pub fn set(&mut self, tick: u32) {
        let pos = (tick & WINDOW_MASK) as usize;
        let w = pos >> 6;
        self.set_leaf(w, self.leaf(w) | (1u64 << (pos & 63)));
        let s = w >> 6;
        self.set_summary_word(s, self.summary_word(s) | (1u64 << (w & 63)));
    }

    #[inline(always)]
    pub fn clear(&mut self, tick: u32) {
        let pos = (tick & WINDOW_MASK) as usize;
        let w = pos >> 6;
        let leaf = self.leaf(w) & !(1u64 << (pos & 63));
        self.set_leaf(w, leaf);
        if leaf == 0 {
            let s = w >> 6;
            self.set_summary_word(s, self.summary_word(s) & !(1u64 << (w & 63)));
        }
    }

    /// Lowest set physical position in `[from, to)`, no wrap.
    #[inline(always)]
    fn scan_up(&self, from: usize, to: usize) -> Option<usize> {
        if from >= to {
            return None;
        }
        let first_word = from >> 6;
        let m = self.leaf(first_word) & (u64::MAX << (from & 63));
        if m != 0 {
            let p = (first_word << 6) + m.trailing_zeros() as usize;
            return if p < to { Some(p) } else { None };
        }
        let last_word = (to - 1) >> 6;
        let mut w = first_word + 1;
        while w <= last_word {
            let s = w >> 6;
            let sm = self.summary_word(s) & (u64::MAX << (w & 63));
            if sm == 0 {
                w = (s + 1) << 6;
                continue;
            }
            let cand = (s << 6) + sm.trailing_zeros() as usize;
            if cand > last_word {
                return None;
            }
            let p = (cand << 6) + self.leaf(cand).trailing_zeros() as usize;
            return if p < to { Some(p) } else { None };
        }
        None
    }

    /// Highest set physical position in `[down_to, from]`, no wrap.
    #[inline(always)]
    fn scan_down(&self, from: usize, down_to: usize) -> Option<usize> {
        let first_word = from >> 6;
        let m = self.leaf(first_word) & (u64::MAX >> (63 - (from & 63)));
        if m != 0 {
            let p = (first_word << 6) + 63 - m.leading_zeros() as usize;
            return if p >= down_to { Some(p) } else { None };
        }
        if first_word == 0 {
            return None;
        }
        let down_word = down_to >> 6;
        let mut w = first_word - 1;
        loop {
            let s = w >> 6;
            let sm = self.summary_word(s) & (u64::MAX >> (63 - (w & 63)));
            if sm == 0 {
                if s == 0 {
                    return None;
                }
                // Step to the last leaf word of the previous summary group.
                // `w` and `down_word` are both leaf-word indices.
                w = (s << 6) - 1;
                if w < down_word {
                    return None;
                }
                continue;
            }
            let cand = (s << 6) + 63 - sm.leading_zeros() as usize;
            if cand < down_word {
                return None;
            }
            let p = (cand << 6) + 63 - self.leaf(cand).leading_zeros() as usize;
            return if p >= down_to { Some(p) } else { None };
        }
    }

    /// Lowest occupied tick with `tick >= from`, in tick order, given the
    /// current window `[anchor, anchor + WINDOW - 1]`. `from` must lie in the
    /// window.
    #[inline(always)]
    pub fn next_at_or_after(&self, anchor: u32, from: u32) -> Option<u32> {
        let rel = from.wrapping_sub(anchor);
        let start_pos = (from & WINDOW_MASK) as usize;
        let count = WINDOW_TICKS - rel as usize;
        let seg_end = core::cmp::min(WINDOW_TICKS, start_pos + count);
        if let Some(p) = self.scan_up(start_pos, seg_end) {
            return Some(from + (p - start_pos) as u32);
        }
        let remaining = count - (seg_end - start_pos);
        if remaining > 0 {
            if let Some(p) = self.scan_up(0, remaining) {
                return Some(from + (seg_end - start_pos) as u32 + p as u32);
            }
        }
        None
    }

    /// Highest occupied tick with `tick <= from`, in tick order, given the
    /// current window. `from` must lie in the window.
    #[inline(always)]
    pub fn prev_at_or_before(&self, anchor: u32, from: u32) -> Option<u32> {
        let rel = from.wrapping_sub(anchor);
        let start_pos = (from & WINDOW_MASK) as usize;
        let count = rel as usize + 1;
        let seg_start = start_pos + 1 - core::cmp::min(count, start_pos + 1);
        if let Some(p) = self.scan_down(start_pos, seg_start) {
            return Some(from - (start_pos - p) as u32);
        }
        let remaining = count - (start_pos + 1 - seg_start);
        if remaining > 0 {
            if let Some(p) = self.scan_down(WINDOW_TICKS - 1, WINDOW_TICKS - remaining) {
                return Some(from - (start_pos + 1) as u32 - (WINDOW_TICKS - 1 - p) as u32);
            }
        }
        None
    }

    /// Best ask = lowest occupied tick in the window.
    #[inline(always)]
    pub fn lowest(&self, anchor: u32) -> Option<u32> {
        self.next_at_or_after(anchor, anchor)
    }

    /// Best bid = highest occupied tick in the window.
    #[inline(always)]
    pub fn highest(&self, anchor: u32) -> Option<u32> {
        self.prev_at_or_before(anchor, anchor + WINDOW_MASK)
    }

    /// True when the 64-tick word containing `tick_base` has no occupied
    /// level. Lets recenter skip a whole vacated word with one load instead
    /// of 64 per-tick probes.
    #[inline(always)]
    fn word_empty(&self, tick_base: u32) -> bool {
        self.leaf(((tick_base & WINDOW_MASK) >> 6) as usize) == 0
    }
}

/// # Level
///
/// One price level: FIFO queue head/tail into the order pool plus the total
/// resting lots. A level is fully zeroed the moment it empties; the bitmap bit
/// is the single source of truth for occupancy.
#[repr(C)]
pub struct Level {
    head: [u8; 2],
    tail: [u8; 2],
    total_lots: [u8; 8],
    _padding: [u8; 4],
}

impl Level {
    #[inline(always)]
    pub fn head(&self) -> u16 {
        u16::from_le_bytes(self.head)
    }

    #[inline(always)]
    pub fn tail(&self) -> u16 {
        u16::from_le_bytes(self.tail)
    }

    #[cfg(feature = "test-utils")]
    #[inline(always)]
    pub fn total_lots(&self) -> u64 {
        u64::from_le_bytes(self.total_lots)
    }

    #[inline(always)]
    pub fn set_head(&mut self, v: u16) {
        self.head = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_tail(&mut self, v: u16) {
        self.tail = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn add_lots(&mut self, lots: u32) {
        self.total_lots =
            (u64::from_le_bytes(self.total_lots).wrapping_add(lots as u64)).to_le_bytes();
    }

    #[inline(always)]
    pub fn sub_lots(&mut self, lots: u32) {
        self.total_lots =
            (u64::from_le_bytes(self.total_lots).wrapping_sub(lots as u64)).to_le_bytes();
    }

    #[inline(always)]
    pub fn zero(&mut self) {
        self.head = [0; 2];
        self.tail = [0; 2];
        self.total_lots = [0; 8];
        self._padding = [0; 4];
    }
}

/// # OrderNode
///
/// 30 bytes exactly. `prev`/`next` are the level FIFO links;
/// `maker_prev`/`maker_next` thread the owning seat's order chain (sorted by
/// (side, tick)). Both lists are doubly linked, so a node is removed from
/// both in O(1) the moment its order ends — a node is either LIVE (on a level
/// and a chain) or FREE (`seq == FREED_SEQ`, on the free list via `next`).
#[repr(C)]
pub struct OrderNode {
    prev: [u8; 2],
    next: [u8; 2],
    maker_prev: [u8; 2],
    maker_next: [u8; 2],
    tick: [u8; 4],
    maker: [u8; 2], // low 15 bits = seat, high bit = ask
    lots: [u8; 4],
    expiry_slot: [u8; 4],
    seq: [u8; 8],
}

const MAKER_INDEX_MASK: u16 = 0x7FFF;
const ASK_SIDE_FLAG: u16 = 0x8000;

impl OrderNode {
    #[inline(always)]
    pub fn prev(&self) -> u16 {
        u16::from_le_bytes(self.prev)
    }

    #[inline(always)]
    pub fn next(&self) -> u16 {
        u16::from_le_bytes(self.next)
    }

    #[inline(always)]
    pub fn maker_prev(&self) -> u16 {
        u16::from_le_bytes(self.maker_prev)
    }

    #[inline(always)]
    pub fn maker_next(&self) -> u16 {
        u16::from_le_bytes(self.maker_next)
    }

    #[inline(always)]
    pub fn tick(&self) -> u32 {
        u32::from_le_bytes(self.tick)
    }

    #[inline(always)]
    pub fn maker(&self) -> u16 {
        u16::from_le_bytes(self.maker) & MAKER_INDEX_MASK
    }

    #[inline(always)]
    pub fn side(&self) -> Side {
        if u16::from_le_bytes(self.maker) & ASK_SIDE_FLAG == 0 {
            Side::Bid
        } else {
            Side::Ask
        }
    }

    #[inline(always)]
    pub fn lots(&self) -> u32 {
        u32::from_le_bytes(self.lots)
    }

    #[inline(always)]
    pub fn expiry_slot(&self) -> u32 {
        u32::from_le_bytes(self.expiry_slot)
    }

    #[inline(always)]
    pub fn seq(&self) -> u64 {
        u64::from_le_bytes(self.seq)
    }

    #[inline(always)]
    pub fn is_expired(&self, slot: u64) -> bool {
        let e = self.expiry_slot();
        e != 0 && (e as u64) <= slot
    }

    #[inline(always)]
    pub fn set_prev(&mut self, v: u16) {
        self.prev = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_next(&mut self, v: u16) {
        self.next = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_maker_prev(&mut self, v: u16) {
        self.maker_prev = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_maker_next(&mut self, v: u16) {
        self.maker_next = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_tick(&mut self, v: u32) {
        self.tick = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_maker_and_side(&mut self, maker: u16, side: Side) {
        let side_flag = match side {
            Side::Bid => 0,
            Side::Ask => ASK_SIDE_FLAG,
        };
        self.maker = ((maker & MAKER_INDEX_MASK) | side_flag).to_le_bytes();
    }

    #[inline(always)]
    pub fn set_lots(&mut self, v: u32) {
        self.lots = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_expiry_slot(&mut self, v: u32) {
        self.expiry_slot = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_seq(&mut self, v: u64) {
        self.seq = v.to_le_bytes();
    }
}

/// # Seat
///
/// Internal balances (atoms) plus the head of the seat's order chain. Every
/// fill, cancel, refund, and eviction is arithmetic on these fields — zero CPI
/// anywhere in the matching path. A free seat slot has `owner == [0; 32]`,
/// and `order_count` counts exactly the seat's live orders.
#[repr(C)]
pub struct Seat {
    owner: [u8; 32],
    base_free: [u8; 8],
    quote_free: [u8; 8],
    base_locked: [u8; 8],
    quote_locked: [u8; 8],
    orders_head: [u8; 2],
    order_count: [u8; 2],
    _padding: [u8; 12],
}

impl Seat {
    #[inline(always)]
    pub fn owner(&self) -> &[u8; 32] {
        &self.owner
    }

    #[inline(always)]
    pub fn is_free(&self) -> bool {
        self.owner == [0u8; 32]
    }

    #[inline(always)]
    pub fn base_free(&self) -> u64 {
        u64::from_le_bytes(self.base_free)
    }

    #[inline(always)]
    pub fn quote_free(&self) -> u64 {
        u64::from_le_bytes(self.quote_free)
    }

    #[inline(always)]
    pub fn base_locked(&self) -> u64 {
        u64::from_le_bytes(self.base_locked)
    }

    #[inline(always)]
    pub fn quote_locked(&self) -> u64 {
        u64::from_le_bytes(self.quote_locked)
    }

    #[inline(always)]
    pub fn orders_head(&self) -> u16 {
        u16::from_le_bytes(self.orders_head)
    }

    #[inline(always)]
    pub fn order_count(&self) -> u16 {
        u16::from_le_bytes(self.order_count)
    }

    #[inline(always)]
    pub fn set_owner(&mut self, v: Pubkey) {
        self.owner = v;
    }

    #[inline(always)]
    pub fn set_orders_head(&mut self, v: u16) {
        self.orders_head = v.to_le_bytes();
    }

    /// The count is exact — every insert pairs with one removal — so plain
    /// wrapping arithmetic is used on purpose: a bug that decrements past
    /// zero produces a huge count that refuses further inserts (loudly),
    /// instead of a clamp that hides the miscount.
    #[inline(always)]
    pub fn inc_order_count(&mut self) {
        self.order_count = u16::from_le_bytes(self.order_count)
            .wrapping_add(1)
            .to_le_bytes();
    }

    #[inline(always)]
    pub fn dec_order_count(&mut self) {
        self.order_count = u16::from_le_bytes(self.order_count)
            .wrapping_sub(1)
            .to_le_bytes();
    }

    #[inline(always)]
    pub fn set_base_free(&mut self, v: u64) {
        self.base_free = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_quote_free(&mut self, v: u64) {
        self.quote_free = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_base_locked(&mut self, v: u64) {
        self.base_locked = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn set_quote_locked(&mut self, v: u64) {
        self.quote_locked = v.to_le_bytes();
    }

    /// Move `atoms` from free to locked on the given asset.
    #[inline(always)]
    pub fn lock(&mut self, base: bool, atoms: u64) -> Result<(), ProgramError> {
        if base {
            let free = self.base_free();
            if free < atoms {
                return Err(ClobError::InsufficientBalance.into());
            }
            self.set_base_free(free - atoms);
            self.set_base_locked(self.base_locked().wrapping_add(atoms));
        } else {
            let free = self.quote_free();
            if free < atoms {
                return Err(ClobError::InsufficientBalance.into());
            }
            self.set_quote_free(free - atoms);
            self.set_quote_locked(self.quote_locked().wrapping_add(atoms));
        }
        Ok(())
    }

    /// Move `atoms` from locked back to free on the given asset.
    ///
    /// Checked: a locked balance smaller than the refund means the book is
    /// corrupt, and the instruction must abort rather than wrap the
    /// subtraction and mint the difference. The credit side stays wrapping —
    /// free + locked is bounded by real deposits, which cannot exceed the
    /// mint's u64 supply.
    #[inline(always)]
    pub fn unlock(&mut self, base: bool, atoms: u64) -> Result<(), ProgramError> {
        if base {
            let locked = self.base_locked();
            if unlikely(locked < atoms) {
                return Err(ClobError::InvalidState.into());
            }
            self.set_base_locked(locked - atoms);
            self.set_base_free(self.base_free().wrapping_add(atoms));
        } else {
            let locked = self.quote_locked();
            if unlikely(locked < atoms) {
                return Err(ClobError::InvalidState.into());
            }
            self.set_quote_locked(locked - atoms);
            self.set_quote_free(self.quote_free().wrapping_add(atoms));
        }
        Ok(())
    }
}

/// One quote inside a MassQuote payload: `tick u32 | lots u32 | flags u8`.
#[derive(Clone, Copy)]
pub struct Quote {
    pub tick: u32,
    pub lots: u32,
    pub flags: u8,
}

impl Quote {
    pub const LEN: usize = 9;

    /// Post-only behavior on cross: false = slide to one tick off the opposing
    /// best (default), true = reject the whole instruction.
    #[inline(always)]
    pub fn reject_on_cross(&self) -> bool {
        self.flags & 0b0000_0001 != 0
    }

    /// # Safety
    ///
    /// `data` must hold at least `(i + 1) * Quote::LEN` bytes. The instruction
    /// data codec validates the exact payload length once in `try_from`; the
    /// merge indexes strictly below the validated count, so per-field bounds
    /// checks here would be pure waste.
    #[inline(always)]
    pub unsafe fn parse(data: &[u8], i: usize) -> Quote {
        let p = data.as_ptr().add(i * Self::LEN);
        Quote {
            tick: u32::from_le_bytes(*(p as *const [u8; 4])),
            lots: u32::from_le_bytes(*(p.add(4) as *const [u8; 4])),
            flags: *p.add(8),
        }
    }
}

/// # BookRefMut
///
/// Typed mutable view over the whole market account, carved once per
/// instruction. All matching logic lives here as Execution Helpers operating on
/// pure bytes — no `AccountInfo`, no CPI, no syscalls (the slot is passed in).
pub struct BookRefMut<'a> {
    pub header: &'a mut Market,
    pub bid_bitmap: &'a mut Bitmap,
    pub ask_bitmap: &'a mut Bitmap,
    pub levels: &'a mut [Level],
    pub seats: &'a mut [Seat],
    pub pool: &'a mut [OrderNode],
}

impl<'a> BookRefMut<'a> {
    /// # Safety
    ///
    /// The caller must have validated the account via `Market::check` (owner,
    /// length matching pool capacity, version). All region structs have
    /// alignment 1, so the raw carving below is sound at any offset.
    #[inline(always)]
    pub unsafe fn from_bytes_unchecked_mut(data: &'a mut [u8]) -> Self {
        let ptr = data.as_mut_ptr();
        let capacity = Market::from_bytes_unchecked(data).order_pool_capacity() as usize;
        Self {
            header: &mut *(ptr as *mut Market),
            bid_bitmap: &mut *(ptr.add(BID_BITMAP_OFFSET) as *mut Bitmap),
            ask_bitmap: &mut *(ptr.add(ASK_BITMAP_OFFSET) as *mut Bitmap),
            levels: core::slice::from_raw_parts_mut(
                ptr.add(LEVELS_OFFSET) as *mut Level,
                WINDOW_TICKS,
            ),
            seats: core::slice::from_raw_parts_mut(ptr.add(SEATS_OFFSET) as *mut Seat, MAX_SEATS),
            pool: core::slice::from_raw_parts_mut(ptr.add(POOL_OFFSET) as *mut OrderNode, capacity),
        }
    }

    /* Bitmap helpers */

    #[inline(always)]
    fn bitmap(&mut self, side: Side) -> &mut Bitmap {
        match side {
            Side::Bid => self.bid_bitmap,
            Side::Ask => self.ask_bitmap,
        }
    }

    /// Fast occupancy probe — one bit load instead of a level access. Not a
    /// safety gate: NIL == 0 and node 0 is reserved, so an empty level is
    /// unambiguous from `head() == NIL` alone; bitmap and levels agree by
    /// invariant I4.
    #[inline(always)]
    fn is_occupied(&self, side: Side, tick: u32) -> bool {
        match side {
            Side::Bid => self.bid_bitmap.get(tick),
            Side::Ask => self.ask_bitmap.get(tick),
        }
    }

    #[inline(always)]
    pub fn best_bid(&self) -> Option<u32> {
        self.bid_bitmap.highest(self.header.anchor_tick())
    }

    #[inline(always)]
    pub fn best_ask(&self) -> Option<u32> {
        self.ask_bitmap.lowest(self.header.anchor_tick())
    }

    /// Locked atoms for an order: bids lock quote (notional), asks lock base.
    #[inline(always)]
    fn locked_atoms(&self, side: Side, tick: u32, lots: u32) -> (bool, u64) {
        match side {
            Side::Bid => (false, self.header.notional(tick, lots)),
            Side::Ask => (true, self.header.base_atoms(lots)),
        }
    }

    /* Node pool */

    #[inline(always)]
    fn alloc_node(&mut self) -> Result<u16, ProgramError> {
        let idx = self.header.free_head();
        if idx == NIL {
            return Err(ClobError::PoolFull.into());
        }
        self.header.set_free_head(self.pool[idx as usize].next());
        self.header.set_free_count(self.header.free_count() - 1);
        Ok(idx)
    }

    #[inline(always)]
    fn free_node(&mut self, idx: u16) {
        let head = self.header.free_head();
        let node = &mut self.pool[idx as usize];
        node.set_next(head);
        node.set_seq(FREED_SEQ);
        node.set_lots(0);
        self.header.set_free_head(idx);
        self.header.set_free_count(self.header.free_count() + 1);
    }

    /// Thread nodes `[from, to)` onto the free list (market init). Callers
    /// start at 1: node 0 is the reserved NIL node and is never allocated.
    pub fn thread_free_list(&mut self, from: u32, to: u32) {
        let mut head = self.header.free_head();
        let mut i = to;
        while i > from {
            i -= 1;
            let node = &mut self.pool[i as usize];
            node.set_next(head);
            node.set_seq(FREED_SEQ);
            node.set_lots(0);
            head = i as u16;
        }
        self.header.set_free_head(head);
        self.header
            .set_free_count(self.header.free_count() + (to - from));
    }

    /* Level FIFO */

    /// Append a live node to its level FIFO tail; sets the bitmap bit if the
    /// level was empty.
    #[inline(always)]
    fn fifo_append(&mut self, side: Side, tick: u32, idx: u16, lots: u32) {
        let lvl = (tick & WINDOW_MASK) as usize;
        let occupied = self.bitmap(side).get(tick);
        let level = &mut self.levels[lvl];
        if occupied {
            let tail = level.tail();
            self.pool[tail as usize].set_next(idx);
            let node = &mut self.pool[idx as usize];
            node.set_prev(tail);
            node.set_next(NIL);
            level.set_tail(idx);
            level.add_lots(lots);
        } else {
            level.set_head(idx);
            level.set_tail(idx);
            level.add_lots(lots);
            let node = &mut self.pool[idx as usize];
            node.set_prev(NIL);
            node.set_next(NIL);
            self.bitmap(side).set(tick);
        }
    }

    /// Unlink a live node from its level FIFO, updating the level total and
    /// clearing/zeroing the level if it empties. Does NOT touch the seat chain
    /// or funds.
    #[inline(always)]
    fn fifo_unlink(&mut self, side: Side, idx: u16) {
        let (tick, prev, next, lots) = {
            let node = &self.pool[idx as usize];
            (node.tick(), node.prev(), node.next(), node.lots())
        };
        let lvl = (tick & WINDOW_MASK) as usize;
        if prev != NIL {
            self.pool[prev as usize].set_next(next);
        } else {
            self.levels[lvl].set_head(next);
        }
        if next != NIL {
            self.pool[next as usize].set_prev(prev);
        } else {
            self.levels[lvl].set_tail(prev);
        }
        if self.levels[lvl].head() == NIL {
            self.levels[lvl].zero();
            self.bitmap(side).clear(tick);
        } else {
            self.levels[lvl].sub_lots(lots);
        }
    }

    /// End a live order and refund its remaining lock: unlink from the level
    /// FIFO and the seat chain (both O(1)), credit the maker, free the node.
    ///
    /// Fallible only on corrupt state (a lock smaller than the refund); on the
    /// error the instruction reverts, so nothing is half-removed.
    #[inline(always)]
    fn kill_order(&mut self, idx: u16) -> Result<(), ProgramError> {
        let (side, tick, lots, maker) = {
            let node = &self.pool[idx as usize];
            (node.side(), node.tick(), node.lots(), node.maker())
        };
        self.fifo_unlink(side, idx);
        let (base, atoms) = self.locked_atoms(side, tick, lots);
        self.seats[maker as usize].unlock(base, atoms)?;
        self.chain_unlink(idx);
        Ok(())
    }

    /* Seat order chain (doubly linked, sorted by (side, tick)) */

    /// Sort key: bids before asks, then ascending tick.
    #[inline(always)]
    fn chain_key(&self, idx: u16) -> u64 {
        let node = &self.pool[idx as usize];
        ((node.side() as u64) << 32) | node.tick() as u64
    }

    /// Unlink `idx` from its seat chain via its own back-link — O(1), no
    /// predecessor hint, no walk — and free it.
    #[inline(always)]
    fn chain_unlink(&mut self, idx: u16) {
        let (seat, prev, next) = {
            let node = &self.pool[idx as usize];
            (node.maker(), node.maker_prev(), node.maker_next())
        };
        if prev == NIL {
            self.seats[seat as usize].set_orders_head(next);
        } else {
            self.pool[prev as usize].set_maker_next(next);
        }
        if next != NIL {
            self.pool[next as usize].set_maker_prev(prev);
        }
        self.seats[seat as usize].dec_order_count();
        self.free_node(idx);
    }

    /// Splice `idx` between `prev` and `next` on the seat's chain and count it.
    #[inline(always)]
    fn chain_splice(&mut self, seat: u16, prev: u16, next: u16, idx: u16) {
        {
            let node = &mut self.pool[idx as usize];
            node.set_maker_prev(prev);
            node.set_maker_next(next);
        }
        if prev == NIL {
            self.seats[seat as usize].set_orders_head(idx);
        } else {
            self.pool[prev as usize].set_maker_next(idx);
        }
        if next != NIL {
            self.pool[next as usize].set_maker_prev(idx);
        }
        self.seats[seat as usize].inc_order_count();
    }

    /// Refuse the insert when the seat is at its order cap. The count is
    /// exact (no dead nodes), so this is a load and a compare.
    #[inline(always)]
    fn ensure_order_slot(&self, seat: u16) -> Result<(), ProgramError> {
        if unlikely(self.seats[seat as usize].order_count() >= MAX_ORDERS_PER_SEAT) {
            return Err(ClobError::TooManyOrders.into());
        }
        Ok(())
    }

    /// Insert a node into the seat chain at its sorted position.
    ///
    /// The walk is bounded by the chain length, which `ensure_order_slot`
    /// caps at MAX_ORDERS_PER_SEAT by construction; a corrupt cycle would
    /// exhaust compute and abort the transaction with no state committed.
    fn chain_insert_sorted(&mut self, seat: u16, idx: u16) {
        let key = self.chain_key(idx);
        let mut prev = NIL;
        let mut cur = self.seats[seat as usize].orders_head();
        while cur != NIL && self.chain_key(cur) < key {
            prev = cur;
            cur = self.pool[cur as usize].maker_next();
        }
        self.chain_splice(seat, prev, cur, idx);
    }

    /* Order lifecycle */

    /// Post a resting order: crossing check, funds lock, level append, sorted
    /// chain insert. Returns the node index.
    ///
    /// `slide`: when the tick crosses the opposing best, rest one tick off it
    /// instead of failing. Returns WouldCross when `!slide` and crossing.
    /// Add or amend one quote, leaving the maker's other quotes alone.
    ///
    /// A maker holds at most one order per (side, tick), so a tick they
    /// already quote is amended rather than duplicated, on the same terms the
    /// bulk merge uses: a smaller size shrinks in place and keeps its place in
    /// the queue, a larger one re-posts at the back.
    pub fn upsert_order(
        &mut self,
        side: Side,
        tick: u32,
        lots: u32,
        maker: u16,
        expiry_slot: u32,
        slide: bool,
    ) -> Result<u16, ProgramError> {
        if unlikely(lots == 0) {
            let tick = self.check_or_slide_vs(side, tick, false, None)?;
            let cur = self
                .find_quote(maker, side, tick)
                .ok_or(ClobError::OrderNotFound)?;
            self.kill_order(cur)?;
            return Ok(cur);
        }
        // Bound lots before locked_atoms's multiply, not after it.
        if unlikely(lots > self.header.max_lots_per_order()) {
            return Err(ClobError::InvalidLots.into());
        }
        let tick = self.check_or_slide(side, tick, slide)?;

        if let Some(cur) = self.find_quote(maker, side, tick) {
            let node_lots = self.pool[cur as usize].lots();
            if lots <= node_lots {
                let diff = node_lots - lots;
                if diff > 0 {
                    let (base, atoms) = self.locked_atoms(side, tick, diff);
                    self.seats[maker as usize].unlock(base, atoms)?;
                    self.pool[cur as usize].set_lots(lots);
                    self.levels[(tick & WINDOW_MASK) as usize].sub_lots(diff);
                }
                self.pool[cur as usize].set_expiry_slot(expiry_slot);
                return Ok(cur);
            }
            self.kill_order(cur)?;
        }

        self.ensure_order_slot(maker)?;
        let (base, atoms) = self.locked_atoms(side, tick, lots);
        self.seats[maker as usize].lock(base, atoms)?;
        let idx = self.book_insert(side, tick, lots, maker, expiry_slot)?;
        self.chain_insert_sorted(maker, idx);
        Ok(idx)
    }

    /// Locate a maker's live quote at `(side, tick)`. The chain is sorted by
    /// (side, tick), so the walk stops as soon as it passes the key.
    fn find_quote(&self, maker: u16, side: Side, tick: u32) -> Option<u16> {
        let key = ((side as u64) << 32) | tick as u64;
        let mut cur = self.seats[maker as usize].orders_head();
        while cur != NIL {
            let k = self.chain_key(cur);
            if k == key {
                return Some(cur);
            }
            if k > key {
                return None;
            }
            cur = self.pool[cur as usize].maker_next();
        }
        None
    }

    /// Crossing check for a resting order; optionally slides to one tick off
    /// the opposing best. Also validates the window bounds.
    #[inline(always)]
    fn check_or_slide(&mut self, side: Side, tick: u32, slide: bool) -> Result<u32, ProgramError> {
        let opposing_best = match side {
            Side::Bid => self.best_ask(),
            Side::Ask => self.best_bid(),
        };
        self.check_or_slide_vs(side, tick, slide, opposing_best)
    }

    /// Crossing check against a pre-computed opposing best. During a one-side
    /// mass_quote merge the opposing best is invariant (inserting bids cannot
    /// move the best ask), so the caller hoists ONE scan out of N inserts.
    #[inline(always)]
    fn check_or_slide_vs(
        &self,
        side: Side,
        tick: u32,
        slide: bool,
        opposing_best: Option<u32>,
    ) -> Result<u32, ProgramError> {
        let anchor = self.header.anchor_tick();
        if tick < anchor || tick > anchor + WINDOW_MASK || tick > self.header.tick_limit() {
            return Err(ClobError::TickOutOfWindow.into());
        }
        // Price 0 is not a tradeable price.
        if unlikely(tick == 0) {
            return Err(ClobError::TickOutOfWindow.into());
        }
        let adjusted = match side {
            Side::Bid => match opposing_best {
                Some(ba) if tick >= ba => {
                    if !slide {
                        return Err(ClobError::WouldCross.into());
                    }
                    if ba == anchor {
                        return Err(ClobError::TickOutOfWindow.into());
                    }
                    ba - 1
                }
                _ => tick,
            },
            Side::Ask => match opposing_best {
                Some(bb) if tick <= bb => {
                    if !slide {
                        return Err(ClobError::WouldCross.into());
                    }
                    if bb == anchor + WINDOW_MASK {
                        return Err(ClobError::TickOutOfWindow.into());
                    }
                    bb + 1
                }
                _ => tick,
            },
        };
        Ok(adjusted)
    }

    /// Allocate + initialize a node and append it to its level. The seat
    /// chain links are set by the caller's splice, and funds by the caller's
    /// lock.
    fn book_insert(
        &mut self,
        side: Side,
        tick: u32,
        lots: u32,
        maker: u16,
        expiry_slot: u32,
    ) -> Result<u16, ProgramError> {
        if lots == 0 || lots > self.header.max_lots_per_order() {
            return Err(ClobError::InvalidLots.into());
        }
        let idx = self.alloc_node()?;
        let seq = self.header.next_order_id();
        {
            let node = &mut self.pool[idx as usize];
            node.set_tick(tick);
            node.set_maker_and_side(maker, side);
            node.set_lots(lots);
            node.set_expiry_slot(expiry_slot);
            node.set_seq(seq);
        }
        self.fifo_append(side, tick, idx, lots);
        Ok(idx)
    }

    /// Cancel a single order by pool index — O(1), no hint required: the
    /// node's own back-links locate it on both of its lists.
    ///
    /// `expected_seq` must equal the node's stored seq: pool slots are
    /// reused, and without this check a stale cancel could kill a stranger's
    /// order that now lives in the same slot (the classic ABA hazard). A
    /// freed node carries `FREED_SEQ`, which no live id can equal, so the seq
    /// comparison alone also rejects already-ended orders.
    pub fn cancel_order(
        &mut self,
        seat: u16,
        idx: u16,
        expected_seq: u64,
    ) -> Result<(), ProgramError> {
        if idx == NIL || idx as usize >= self.pool.len() {
            return Err(ClobError::OrderNotFound.into());
        }
        {
            let node = &self.pool[idx as usize];
            if node.seq() != expected_seq || node.maker() != seat {
                return Err(ClobError::OrderNotFound.into());
            }
        }
        self.kill_order(idx)
    }

    /* Matching */

    /// Match the taker against the opposing side of the book.
    ///
    /// - `taker_side`: Bid = buying base (consumes asks, ascending), Ask =
    ///   selling base (consumes bids, descending).
    /// - `limit_tick`: worst acceptable price.
    /// - `max_lots`: lot budget (must be <= max_lots_per_order).
    /// - `max_quote`: quote budget including fee (u64::MAX = unbounded); used
    ///   by exact-in swaps on the buy side.
    /// - `taker_seat`: NO_SEAT for seatless flow (swap) — no self-trade
    ///   concept, and NO_SEAT can never alias a real seat index.
    ///
    /// The walk is two nested loops. The outer loop finds the next occupied
    /// opposing level in price order — incrementally, scanning FROM the level
    /// it just consumed rather than re-running a full best-price scan, since
    /// the next level is usually a few bits away in the same leaf word. The
    /// inner loop drains that level's FIFO front-to-back (price-time
    /// priority): per resting node it settles the maker's seat in place, then
    /// either decrements the node (partial fill) or removes and frees it
    /// (full fill — both unlinks are O(1)).
    ///
    /// Two kinds of nodes are removed for free mid-walk instead of matching:
    /// expired quotes (TTL elapsed — refund the maker and keep going, which is
    /// why makers never race to cancel) and, under the default self-trade
    /// policy, the taker's own resting orders. `taker_seat` is `NO_SEAT` for
    /// the seatless flow (`Swap`), which can never equal a node's maker.
    ///
    /// Taker seat balances are NOT touched here — callers settle from the
    /// returned `TakeResult` and abort on insufficient funds, reverting the
    /// whole instruction. The taker fee accrues on the market; makers always
    /// receive/pay the raw notional.
    pub(crate) fn take(&mut self, params: TakeParams) -> Result<TakeResult, ProgramError> {
        let TakeParams {
            side: taker_side,
            limit_tick,
            max_lots,
            max_quote,
            seat: taker_seat,
            slot,
            self_trade: policy,
        } = params;
        if max_lots == 0 || max_lots > self.header.max_lots_per_order() {
            return Err(ClobError::InvalidLots.into());
        }
        let book_side = taker_side.opposite();
        let fee_bps = self.header.fee_bps() as u64;
        let anchor = self.header.anchor_tick();
        let mut res = TakeResult::default();
        let mut remaining = max_lots;
        // Levels are consumed in strict tick order, so after finishing a level
        // the next one is found by scanning FROM it — usually within the same
        // leaf word — instead of a fresh full best-price scan per level.
        let mut last_level: Option<u32> = None;

        'levels: while remaining > 0 {
            let best = match taker_side {
                Side::Bid => {
                    let from = match last_level {
                        None => anchor,
                        Some(t) if t < anchor + WINDOW_MASK => t + 1,
                        Some(_) => break,
                    };
                    match self.ask_bitmap.next_at_or_after(anchor, from) {
                        Some(t) if t <= limit_tick => t,
                        _ => break,
                    }
                }
                Side::Ask => {
                    let from = match last_level {
                        None => anchor + WINDOW_MASK,
                        Some(t) if t > anchor => t - 1,
                        Some(_) => break,
                    };
                    match self.bid_bitmap.prev_at_or_before(anchor, from) {
                        Some(t) if t >= limit_tick => t,
                        _ => break,
                    }
                }
            };
            last_level = Some(best);
            let price_per_lot = (best as u64) * self.header.tick_size();
            // A zero price would divide by zero in the quote budget below and
            // hand out base for nothing. Tick 0 is already excluded when
            // orders are posted; this is the last line of defence.
            if unlikely(price_per_lot == 0) {
                return Err(ClobError::TickOutOfWindow.into());
            }
            let lvl = (best & WINDOW_MASK) as usize;

            loop {
                // The bitmap decides occupancy — an emptied level is zeroed
                // and its head of 0 must never be read as a node index.
                if !self.is_occupied(book_side, best) {
                    continue 'levels;
                }
                let head = self.levels[lvl].head();
                let (node_lots, node_maker) = {
                    let node = &self.pool[head as usize];
                    (node.lots(), node.maker())
                };
                if self.pool[head as usize].is_expired(slot) {
                    self.kill_order(head)?;
                    continue;
                }
                if node_maker == taker_seat {
                    match policy {
                        SelfTradePolicy::Abort => return Err(ClobError::SelfTrade.into()),
                        SelfTradePolicy::CancelResting => {
                            self.kill_order(head)?;
                            continue;
                        }
                    }
                }

                let mut fill = core::cmp::min(node_lots, remaining);
                if max_quote != u64::MAX {
                    // Quote-budgeted (exact-in buy): cost(f) = n + fee(n),
                    // n = f * price_per_lot. Capping the budget before the
                    // multiply is semantically lossless (any single order's
                    // notional is below the cap by the tick_limit invariant)
                    // and keeps the math in u64.
                    let budget = max_quote - res.quote - res.fee;
                    let capped = core::cmp::min(budget, u64::MAX / (FEE_DENOMINATOR + fee_bps));
                    let affordable_notional =
                        capped * FEE_DENOMINATOR / (FEE_DENOMINATOR + fee_bps);
                    let affordable = affordable_notional / price_per_lot;
                    if affordable == 0 {
                        break 'levels;
                    }
                    // Widen rather than narrow: `affordable` can exceed u32,
                    // and a truncating cast would turn a large budget into a
                    // small fill.
                    fill = core::cmp::min(fill as u64, affordable) as u32;
                    if fill == 0 {
                        break 'levels;
                    }
                }

                let notional = price_per_lot * fill as u64;
                let fee = notional * fee_bps / FEE_DENOMINATOR;
                let base_atoms = self.header.base_atoms(fill);

                // Maker settlement: internal balance arithmetic only. The
                // locked-side debits are checked — a lock smaller than the
                // fill is corruption, and aborting beats minting.
                let maker = &mut self.seats[node_maker as usize];
                match book_side {
                    Side::Ask => {
                        // Maker sold base for quote.
                        let locked = maker.base_locked();
                        if unlikely(locked < base_atoms) {
                            return Err(ClobError::InvalidState.into());
                        }
                        maker.set_base_locked(locked - base_atoms);
                        maker.set_quote_free(maker.quote_free().wrapping_add(notional));
                    }
                    Side::Bid => {
                        // Maker bought base with quote.
                        let locked = maker.quote_locked();
                        if unlikely(locked < notional) {
                            return Err(ClobError::InvalidState.into());
                        }
                        maker.set_quote_locked(locked - notional);
                        maker.set_base_free(maker.base_free().wrapping_add(base_atoms));
                    }
                }

                res.lots += fill;
                res.quote += notional;
                res.fee += fee;
                remaining -= fill;

                if fill == node_lots {
                    // Full fill: remove from level and chain, free the node.
                    self.fifo_unlink(book_side, head);
                    self.chain_unlink(head);
                } else {
                    self.pool[head as usize].set_lots(node_lots - fill);
                    self.levels[lvl].sub_lots(fill);
                }
                if remaining == 0 {
                    break 'levels;
                }
            }
        }

        self.header.add_fees_accrued(res.fee);
        Ok(res)
    }

    /// Test/fuzz-only entry point for `take`, which is `pub(crate)` on the
    /// hot path on purpose (its `TakeParams` are an internal calling
    /// convention, not part of the program's account/instruction ABI). This
    /// wrapper exists solely so out-of-crate harnesses (the `fuzz/` targets)
    /// can drive matching directly against a bare buffer with no `AccountInfo`
    /// involved, mirroring how the crate's own tests already reach in via
    /// `test-utils`.
    #[cfg(feature = "test-utils")]
    #[allow(clippy::too_many_arguments)]
    pub fn take_for_test(
        &mut self,
        side: Side,
        limit_tick: u32,
        max_lots: u32,
        max_quote: u64,
        seat: u16,
        slot: u64,
        self_trade: SelfTradePolicy,
    ) -> Result<TakeResult, ProgramError> {
        self.take(TakeParams {
            side,
            limit_tick,
            max_lots,
            max_quote,
            seat,
            slot,
            self_trade,
        })
    }

    /* Mass quote */

    /// Replace the seat's quote set for one side with `quotes` — the maker
    /// hot path. One linear merge of two sorted sequences: the payload
    /// (strictly ascending by tick, enforced here for free) against the
    /// seat's chain region for this side, sorted the same way. Per aligned
    /// pair the outcome is the cheapest state transition that honors
    /// price-time priority:
    ///
    /// - same tick, same lots: refresh the expiry only — the resting order
    ///   never moves, FIFO priority kept (a TTL heartbeat costs ~8 CU/quote)
    /// - same tick, fewer lots: shrink in place — priority kept (exchange
    ///   convention: size down keeps your place, size up does not)
    /// - same tick, more lots: cancel + re-post at the level tail
    /// - tick only in the payload: post it (spliced into the chain at the
    ///   merge position — no sorted re-walk)
    /// - tick only in the chain: cancel it
    ///
    /// A quote whose post-only SLIDE lands on a tick this merge already used
    /// — or on the maker's next resting quote — is DROPPED rather than
    /// stacked (closes F-2): a maker asking for N distinct levels never means
    /// "pile them on one price", and the merge's correctness stands on the
    /// invariant of at most one order per (side, tick) per maker.
    ///
    /// All quotes share the payload's expiry slot: a maker quoting
    /// `expiry = now + 2` is never exposed beyond 2 slots even if it never
    /// sends another transaction.
    ///
    /// The chain cursor `(prev, cur)` enters this side's region and is
    /// returned positioned at the first node past it, so the ask merge starts
    /// where the bid merge stopped instead of re-walking the bid region.
    pub fn mass_quote_side(
        &mut self,
        seat: u16,
        side: Side,
        data: &[u8],
        count: usize,
        expiry_slot: u32,
        cursor: (u16, u16),
    ) -> Result<(u16, u16), ProgramError> {
        // Invariant for the whole merge: this side's inserts/cancels cannot
        // move the OPPOSING best. One scan, N inserts.
        let opposing_best = match side {
            Side::Bid => self.best_ask(),
            Side::Ask => self.best_bid(),
        };
        let (mut prev, mut cur) = cursor;
        let mut qi = 0usize;
        let mut last_tick = 0u32;
        // Highest tick this merge has placed or kept on this side. A slid
        // insert at or below it would stack on our own quote — rejected.
        let mut last_used = 0u32;

        loop {
            let cur_info = if cur != NIL {
                let t = self.pool[cur as usize].tick();
                // The chain is sorted bids-then-asks, so the first node with
                // the other stored side ends this region.
                if self.pool[cur as usize].side() == side {
                    Some(t)
                } else {
                    None
                }
            } else {
                None
            };

            let quote = if qi < count {
                // Safety: qi < count and the codec validated count * LEN bytes.
                let q = unsafe { Quote::parse(data, qi) };
                if qi > 0 && q.tick <= last_tick {
                    return Err(ClobError::PayloadNotSorted.into());
                }
                if q.lots == 0 || q.lots > self.header.max_lots_per_order() {
                    return Err(ClobError::InvalidLots.into());
                }
                Some(q)
            } else {
                None
            };

            match (cur_info, quote) {
                (None, None) => break,
                (Some(_), None) => {
                    // Chain quote not in payload: cancel it. Read the link
                    // BEFORE the kill frees the node.
                    let next = self.pool[cur as usize].maker_next();
                    self.kill_order(cur)?;
                    cur = next;
                }
                (None, Some(q)) => {
                    // Payload quote past the chain region: insert.
                    self.mass_quote_insert(
                        seat,
                        side,
                        q,
                        expiry_slot,
                        &mut prev,
                        cur,
                        opposing_best,
                        &mut last_used,
                        None,
                    )?;
                    last_tick = q.tick;
                    qi += 1;
                }
                (Some(ct), Some(q)) => {
                    if ct < q.tick {
                        // Chain quote below payload cursor: cancel it.
                        let next = self.pool[cur as usize].maker_next();
                        self.kill_order(cur)?;
                        cur = next;
                    } else if ct > q.tick {
                        self.mass_quote_insert(
                            seat,
                            side,
                            q,
                            expiry_slot,
                            &mut prev,
                            cur,
                            opposing_best,
                            &mut last_used,
                            Some(ct),
                        )?;
                        last_tick = q.tick;
                        qi += 1;
                    } else {
                        // Same tick: diff lots.
                        let node_lots = self.pool[cur as usize].lots();
                        if q.lots == node_lots {
                            self.pool[cur as usize].set_expiry_slot(expiry_slot);
                        } else if q.lots < node_lots && q.lots > 0 {
                            // Decrease in place: FIFO priority kept.
                            let diff = node_lots - q.lots;
                            let (base, atoms) = self.locked_atoms(side, ct, diff);
                            self.seats[seat as usize].unlock(base, atoms)?;
                            self.pool[cur as usize].set_lots(q.lots);
                            self.pool[cur as usize].set_expiry_slot(expiry_slot);
                            self.levels[(ct & WINDOW_MASK) as usize].sub_lots(diff);
                        } else {
                            // Increase (or zero): cancel + re-post at tail.
                            let next = self.pool[cur as usize].maker_next();
                            self.kill_order(cur)?;
                            cur = next;
                            if q.lots > 0 {
                                self.mass_quote_insert(
                                    seat,
                                    side,
                                    q,
                                    expiry_slot,
                                    &mut prev,
                                    cur,
                                    opposing_best,
                                    &mut last_used,
                                    None,
                                )?;
                            }
                            last_tick = q.tick;
                            qi += 1;
                            continue;
                        }
                        last_used = ct;
                        last_tick = q.tick;
                        qi += 1;
                        prev = cur;
                        cur = self.pool[cur as usize].maker_next();
                    }
                }
            }
        }
        Ok((prev, cur))
    }

    /// Insert one payload quote during a merge: crossing check against the
    /// merge's hoisted opposing best, funds lock, book post, chain splice at
    /// the merge position (no sorted re-walk).
    ///
    /// The F-2 guard: a (possibly slid) tick at or below `last_used`, or
    /// equal to the region's next resting quote, would stack two of the
    /// maker's orders on one price — the whole instruction is rejected. Only
    /// a slid tick can trigger either condition:
    /// the payload is strictly ascending and the merge aligns equal ticks
    /// before ever reaching this insert.
    #[allow(clippy::too_many_arguments)]
    fn mass_quote_insert(
        &mut self,
        seat: u16,
        side: Side,
        q: Quote,
        expiry_slot: u32,
        prev: &mut u16,
        next_in_chain: u16,
        opposing_best: Option<u32>,
        last_used: &mut u32,
        region_next_tick: Option<u32>,
    ) -> Result<(), ProgramError> {
        let tick = self.check_or_slide_vs(side, q.tick, !q.reject_on_cross(), opposing_best)?;
        if unlikely(tick <= *last_used || Some(tick) == region_next_tick) {
            return Err(ClobError::WouldCross.into());
        }
        let (base, atoms) = self.locked_atoms(side, tick, q.lots);
        self.seats[seat as usize].lock(base, atoms)?;
        let idx = self.book_insert(side, tick, q.lots, seat, expiry_slot)?;
        self.chain_splice(seat, *prev, next_in_chain, idx);
        *prev = idx;
        *last_used = tick;
        Ok(())
    }

    /* Hygiene */

    /// Evict every order on one occupied level (refund makers, zero the level).
    /// Used by reanchor. Returns evicted node count.
    fn evict_level(&mut self, side: Side, tick: u32) -> Result<u32, ProgramError> {
        let lvl = (tick & WINDOW_MASK) as usize;
        let mut n = 0u32;
        while self.is_occupied(side, tick) {
            let head = self.levels[lvl].head();
            self.kill_order(head)?;
            n += 1;
        }
        Ok(n)
    }

    /// Move the price window to start at `target`, evicting every order that
    /// falls outside it — each refunded through its maker's seat balance,
    /// which needs no vault and cannot fail for lack of funds (the only error
    /// is a corrupt lock, which aborts the instruction).
    ///
    /// Because the level array is a ring, the slots vacated at the departing
    /// edge simply BECOME the fresh slots at the arriving edge: no memory
    /// moves, only the anchor label changes.
    ///
    /// Eviction is bounded by `max_levels` occupied levels per call, so a long
    /// move is completed by calling again. If the budget runs out mid-word the
    /// anchor stops before that word, leaving the window somewhere between the
    /// old position and the target and the book consistent at every step.
    /// Returns (levels_evicted, new_anchor).
    pub fn reanchor_step(
        &mut self,
        target: u32,
        max_levels: u32,
    ) -> Result<(u32, u32), ProgramError> {
        let anchor = self.header.anchor_tick();
        let max_anchor = self.header.tick_limit().saturating_sub(WINDOW_MASK) & !63u32;
        let ideal = core::cmp::min(core::cmp::max(target & !63u32, 64), max_anchor);

        let mut evicted = 0u32;
        let mut cur = anchor;
        if ideal > cur {
            // Window slides up: vacate words at the bottom. Budget checked
            // once per word, before entering it, so a word is always fully
            // vacated or not touched at all — never left half-evicted.
            while cur < ideal && evicted < max_levels {
                // Empty word on both sides: one load each, skip 64 probes.
                if !self.bid_bitmap.word_empty(cur) || !self.ask_bitmap.word_empty(cur) {
                    let word_end = cur + 64;
                    let mut t = cur;
                    while t < word_end {
                        for side in [Side::Bid, Side::Ask] {
                            if self.bitmap(side).get(t) {
                                evicted += self.evict_level(side, t)?;
                            }
                        }
                        t += 1;
                    }
                }
                cur += 64;
            }
        } else if ideal < cur {
            // Window slides down: vacate words at the top. Same word-atomic
            // budgeting as the slide-up arm above.
            while cur > ideal && evicted < max_levels {
                let top_start = cur + WINDOW_MASK - 63; // last word of current window
                if !self.bid_bitmap.word_empty(top_start) || !self.ask_bitmap.word_empty(top_start)
                {
                    let mut t = top_start;
                    while t <= cur + WINDOW_MASK {
                        for side in [Side::Bid, Side::Ask] {
                            if self.bitmap(side).get(t) {
                                evicted += self.evict_level(side, t)?;
                            }
                        }
                        t += 1;
                    }
                }
                cur -= 64;
            }
        }
        self.header.set_anchor_tick(cur);
        Ok((evicted, cur))
    }

    /// Reap expired orders on one side, walking levels in tick order from
    /// `start_tick`, up to `max_count` orders. Returns reaped count.
    pub fn reap_expired(
        &mut self,
        side: Side,
        start_tick: u32,
        max_count: u32,
        slot: u64,
    ) -> Result<u32, ProgramError> {
        let anchor = self.header.anchor_tick();
        let mut reaped = 0u32;
        // `next_at_or_after` requires its starting tick to lie inside the
        // window, so clamp to both edges.
        let mut tick = core::cmp::min(core::cmp::max(start_tick, anchor), anchor + WINDOW_MASK);
        loop {
            let found = match side {
                Side::Bid => self.bid_bitmap.next_at_or_after(anchor, tick),
                Side::Ask => self.ask_bitmap.next_at_or_after(anchor, tick),
            };
            let t = match found {
                Some(t) => t,
                None => return Ok(reaped),
            };
            // Walk the level FIFO, reaping expired nodes. The link is read
            // BEFORE a kill frees the node.
            let lvl = (t & WINDOW_MASK) as usize;
            let mut cur = self.levels[lvl].head();
            while cur != NIL {
                let next = self.pool[cur as usize].next();
                if self.pool[cur as usize].is_expired(slot) {
                    self.kill_order(cur)?;
                    reaped += 1;
                    if reaped >= max_count {
                        return Ok(reaped);
                    }
                }
                cur = next;
            }
            if t >= anchor + WINDOW_MASK {
                return Ok(reaped);
            }
            tick = t + 1;
        }
    }

    /* Seats */

    /// Claim the first free seat slot for `owner`. Returns the seat index.
    pub fn claim_seat(&mut self, owner: &Pubkey) -> Result<u16, ProgramError> {
        for (i, seat) in self.seats.iter_mut().enumerate() {
            if seat.is_free() {
                seat.set_owner(*owner);
                seat.set_orders_head(NIL);
                return Ok(i as u16);
            }
        }
        Err(ClobError::SeatTableFull.into())
    }

    /// Verify that `seat` exists and is owned by `owner`.
    #[inline(always)]
    pub fn check_seat(&self, seat: u16, owner: &Pubkey) -> Result<(), ProgramError> {
        if unlikely(seat as usize >= MAX_SEATS) {
            return Err(ClobError::InvalidSeatIndex.into());
        }
        if unlikely(self.seats[seat as usize].owner().ne(owner)) {
            return Err(ClobError::InvalidSeatOwner.into());
        }
        Ok(())
    }
}

/// Compile-time layout guards.
const _: () = assert!(size_of::<Market>() == Market::LEN);
const _: () = assert!(size_of::<Config>() == Config::LEN);
const _: () = assert!(size_of::<Bitmap>() == BITMAP_LEN);
const _: () = assert!(size_of::<Level>() == LEVEL_LEN);
const _: () = assert!(size_of::<OrderNode>() == NODE_LEN);
const _: () = assert!(size_of::<Seat>() == SEAT_LEN);

#[cfg(test)]
mod token_program_tests {
    use super::Market;
    use crate::constants::{TOKEN_2022_ID, TOKEN_PROGRAM_2022, TOKEN_PROGRAM_LEGACY};

    #[test]
    fn token_2022_id_decodes_to_a_distinct_nonzero_pubkey() {
        // Guards against a typo'd base58 literal silently decoding to
        // something degenerate (all-zero, or accidentally the legacy Token
        // program id) rather than failing loudly.
        assert_ne!(TOKEN_2022_ID, [0u8; 32]);
        assert_ne!(TOKEN_2022_ID, pinocchio_token::ID);
    }

    #[test]
    fn header_token_program_and_decimals_round_trip() {
        let mut buf = [0u8; Market::LEN];
        let m = unsafe { Market::from_bytes_unchecked_mut(&mut buf) };

        m.set_base_token_program(TOKEN_PROGRAM_2022);
        m.set_quote_token_program(TOKEN_PROGRAM_LEGACY);
        m.set_base_decimals(9);
        m.set_quote_decimals(6);

        assert_eq!(m.base_token_program(), TOKEN_PROGRAM_2022);
        assert_eq!(m.quote_token_program(), TOKEN_PROGRAM_LEGACY);
        assert_eq!(m.base_decimals(), 9);
        assert_eq!(m.quote_decimals(), 6);
        assert_eq!(m.base_token_program_id(), TOKEN_2022_ID);
        assert_eq!(m.quote_token_program_id(), pinocchio_token::ID);
    }

    #[test]
    fn new_header_fields_do_not_disturb_neighboring_fields() {
        // The four new fields were carved out of what used to be 22 bytes of
        // padding, right after `fees_accrued_quote`. Set every neighbor to a
        // recognizable value, write the new fields, and confirm nothing
        // bled across a byte boundary in either direction.
        let mut buf = [0u8; Market::LEN];
        let m = unsafe { Market::from_bytes_unchecked_mut(&mut buf) };

        m.set_max_lots_per_order(0xAABB_CCDD);
        m.add_fees_accrued(0x1122_3344_5566_7788);
        m.set_base_token_program(TOKEN_PROGRAM_2022);
        m.set_quote_token_program(TOKEN_PROGRAM_2022);
        m.set_base_decimals(18);
        m.set_quote_decimals(0);

        assert_eq!(m.max_lots_per_order(), 0xAABB_CCDD);
        assert_eq!(m.take_fees_accrued(), 0x1122_3344_5566_7788);
        assert_eq!(m.base_token_program(), TOKEN_PROGRAM_2022);
        assert_eq!(m.quote_token_program(), TOKEN_PROGRAM_2022);
        assert_eq!(m.base_decimals(), 18);
        assert_eq!(m.quote_decimals(), 0);
    }
}

#[cfg(test)]
mod upsert_order_lots_bound_tests {
    use super::*;

    /// `PlaceLimit` forwards `lots: u32` unchecked, trusting `upsert_order`
    /// to reject an oversized value before any locking or multiply.
    #[test]
    fn oversized_lots_rejected_before_locking_anything() {
        const CAPACITY: u32 = 64;
        const ANCHOR: u32 = 64_000;
        const TICK_SIZE: u64 = 1_000_000_000;
        const MAX_LOTS_PER_ORDER: u32 = 1_000;

        let mut data = vec![0u8; POOL_OFFSET + CAPACITY as usize * NODE_LEN];
        {
            let header = unsafe { Market::from_bytes_unchecked_mut(&mut data) };
            header.set_order_pool_capacity(CAPACITY);
            header.set_anchor_tick(ANCHOR);
            header.set_tick_size(TICK_SIZE);
            header.set_base_lot_size(1_000);
            header.set_max_lots_per_order(MAX_LOTS_PER_ORDER);
            header.set_tick_limit(u32::MAX - 2 * WINDOW_TICKS as u32);
            header.set_free_head(NIL);
        }
        let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
        book.thread_free_list(1, CAPACITY);
        let seat = book.claim_seat(&[1u8; 32]).expect("seat claim");

        let result = book.upsert_order(Side::Bid, ANCHOR, u32::MAX, seat, 0, false);

        assert_eq!(result, Err(ClobError::InvalidLots.into()));
        assert_eq!(book.seats[seat as usize].quote_locked(), 0);
    }
}

#[cfg(test)]
mod reanchor_step_atomic_eviction_tests {
    use super::*;

    fn setup(anchor: u32) -> (Vec<u8>, u16) {
        const CAPACITY: u32 = 64;
        let mut data = vec![0u8; POOL_OFFSET + CAPACITY as usize * NODE_LEN];
        {
            let header = unsafe { Market::from_bytes_unchecked_mut(&mut data) };
            header.set_order_pool_capacity(CAPACITY);
            header.set_anchor_tick(anchor);
            header.set_tick_size(100);
            header.set_base_lot_size(1_000);
            header.set_max_lots_per_order(1_000_000);
            header.set_tick_limit(u32::MAX - 2 * WINDOW_TICKS as u32);
            header.set_free_head(NIL);
        }
        let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };
        book.thread_free_list(1, CAPACITY);
        let seat = book.claim_seat(&[1u8; 32]).expect("seat claim");
        book.seats[seat as usize].set_quote_free(1_000_000_000);
        (data, seat)
    }

    /// Budget of 1 with 2 occupied levels in the first word: the whole word
    /// is still vacated, and the anchor advances past it.
    #[test]
    fn word_is_fully_vacated_or_not_touched_at_all() {
        const ANCHOR: u32 = 64;
        let (mut data, seat) = setup(ANCHOR);
        let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };

        book.upsert_order(Side::Bid, ANCHOR, 10, seat, 0, false)
            .expect("insert tick=ANCHOR");
        book.upsert_order(Side::Bid, ANCHOR + 1, 10, seat, 0, false)
            .expect("insert tick=ANCHOR+1");

        let (evicted, new_anchor) = book.reanchor_step(128, 1).expect("reanchor_step");

        assert_eq!(evicted, 2, "the whole first word is vacated, not just 1");
        assert_eq!(new_anchor, ANCHOR + 64, "anchor advances past the vacated word");
        assert!(!book.bid_bitmap.get(ANCHOR));
        assert!(!book.bid_bitmap.get(ANCHOR + 1));
    }

    /// Budget of 0 touches nothing; anchor and order stay put.
    #[test]
    fn zero_budget_touches_nothing() {
        const ANCHOR: u32 = 64;
        let (mut data, seat) = setup(ANCHOR);
        let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };

        book.upsert_order(Side::Bid, ANCHOR, 10, seat, 0, false)
            .expect("insert tick=ANCHOR");

        let (evicted, new_anchor) = book.reanchor_step(128, 0).expect("reanchor_step");

        assert_eq!(evicted, 0);
        assert_eq!(new_anchor, ANCHOR);
        assert!(book.bid_bitmap.get(ANCHOR));
    }

    /// No surviving order ends up behind the committed anchor.
    #[test]
    fn no_surviving_order_ends_up_behind_the_committed_anchor() {
        const ANCHOR: u32 = 64;
        let (mut data, seat) = setup(ANCHOR);
        let mut book = unsafe { BookRefMut::from_bytes_unchecked_mut(&mut data) };

        book.upsert_order(Side::Bid, ANCHOR, 10, seat, 0, false)
            .expect("insert tick=ANCHOR");
        book.upsert_order(Side::Bid, ANCHOR + 64, 10, seat, 0, false)
            .expect("insert tick=ANCHOR+64");

        let (evicted, new_anchor) = book.reanchor_step(256, 1).expect("reanchor_step");

        assert_eq!(evicted, 1);
        assert_eq!(new_anchor, ANCHOR + 64);
        assert!(!book.bid_bitmap.get(ANCHOR), "first word fully vacated");
        assert!(
            book.bid_bitmap.get(ANCHOR + 64),
            "second word untouched -- its order is exactly at the new anchor, not behind it"
        );
    }
}
