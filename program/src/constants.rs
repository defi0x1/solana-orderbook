/// Number of price ticks in the book window. Power of two: the level array and
/// the bitmaps are rings over tick space (`index = tick & WINDOW_MASK`), so a
/// recenter never moves memory — it only re-labels which slots are valid.
pub const WINDOW_TICKS: usize = 8_192;
pub const WINDOW_MASK: u32 = (WINDOW_TICKS as u32) - 1;

/// Leaf words per bitmap (WINDOW_TICKS / 64) and summary words (leaf words / 64).
pub const BITMAP_LEAF_WORDS: usize = WINDOW_TICKS / 64;
pub const BITMAP_SUMMARY_WORDS: usize = BITMAP_LEAF_WORDS / 64;

/// Fixed seat table size. Sized against the order pool rather than against
/// demand: the pool backs `MAX_SEATS * MAX_ORDERS_PER_SEAT` orders in the
/// worst case, so a seat table far larger than the pool can support is only
/// rent. 128 seats is already several times the number of makers a real book
/// carries, and admission is granted, so the table cannot be squatted.
pub const MAX_SEATS: usize = 128;

/// Maximum resting orders per seat (bounds the sorted-insert walk in
/// `place_limit` and the mass_quote merge pass).
pub const MAX_ORDERS_PER_SEAT: u16 = 128;

/// Order pool index sentinel. Node 0 is reserved at market creation and never
/// allocated, so with NIL = 0 an all-zero byte region *is* the empty state:
/// a zeroed level head, a zeroed seat chain and a zeroed free head all mean
/// "none" definitionally, and no reader can mistake an empty level's head for
/// a live node.
pub const NIL: u16 = 0;

/// Seat-table sentinel for the seatless taker flow (`Swap`). Distinct from
/// `NIL` on purpose: 0 is a real seat index, while u16::MAX can never be one
/// (`MAX_SEATS` = 128), so a seatless taker can never alias seat 0.
pub const NO_SEAT: u16 = u16::MAX;

/// Largest useful pool: every seat at its live-order cap, plus one seat's
/// temporary replacement set during a bid-first full-side MassQuote flip,
/// plus the reserved NIL node.
pub const MAX_POOL_CAPACITY: u32 =
    1 + MAX_SEATS as u32 * MAX_ORDERS_PER_SEAT as u32 + MAX_ORDERS_PER_SEAT as u32;

/// Account region offsets. The order pool is last and its length is fixed at
/// creation, so every region sits at a constant offset for the life of the
/// market.
pub const BID_BITMAP_OFFSET: usize = 320;
pub const ASK_BITMAP_OFFSET: usize = BID_BITMAP_OFFSET + BITMAP_LEN;
pub const LEVELS_OFFSET: usize = ASK_BITMAP_OFFSET + BITMAP_LEN;
pub const SEATS_OFFSET: usize = LEVELS_OFFSET + WINDOW_TICKS * LEVEL_LEN;
pub const POOL_OFFSET: usize = SEATS_OFFSET + MAX_SEATS * SEAT_LEN;

pub const BITMAP_LEN: usize = BITMAP_SUMMARY_WORDS * 8 + BITMAP_LEAF_WORDS * 8; // 1_040
pub const LEVEL_LEN: usize = 16;
pub const SEAT_LEN: usize = 80;
pub const NODE_LEN: usize = 30;

/// Smallest legal market account: fixed regions + a pool of at least 64 nodes
/// (one of which is the reserved node 0).
pub const MIN_POOL_CAPACITY: u32 = 64;
pub const MIN_MARKET_LEN: usize = POOL_OFFSET + MIN_POOL_CAPACITY as usize * NODE_LEN;

// 3 -> 4 added the per-asset token-program/decimals header fields for
// Token-2022 support. Pre-launch, no live markets exist yet, so this bump
// deliberately has no migration path — a version mismatch just fails
// `Market::check` rather than being handled.
pub const MARKET_VERSION: u8 = 4;
pub const CONFIG_VERSION: u8 = 1;

/// Which SPL token program a mint belongs to. Stored per-asset on the market
/// header so Deposit/Withdraw/Swap can route each leg's transfer to the right
/// program instead of assuming one program serves both mints.
pub const TOKEN_PROGRAM_LEGACY: u8 = 0;
pub const TOKEN_PROGRAM_2022: u8 = 1;

/// Token-2022 (Token Extensions) program id. Declared with the same
/// compile-time base58 decode `pinocchio-token` uses for its own `ID`
/// constant, so this isn't a hand-transcribed byte array.
pub const TOKEN_2022_ID: pinocchio::pubkey::Pubkey =
    pinocchio_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Resolve a stored token-program flag back to the program id. `flag` must be
/// one of `TOKEN_PROGRAM_LEGACY` / `TOKEN_PROGRAM_2022` — both only ever
/// written by `CreateMarket` after checking a mint's actual owner.
#[inline(always)]
pub fn token_program_id(flag: u8) -> pinocchio::pubkey::Pubkey {
    if flag == TOKEN_PROGRAM_2022 {
        TOKEN_2022_ID
    } else {
        pinocchio_token::ID
    }
}

/// Config PDA seed: [CONFIG_SEED, authority].
pub const CONFIG_SEED: &[u8] = b"config";
pub const CONFIG_LEN: usize = 128;

/// Fee cap: 5% — anything above is a configuration mistake, not a fee.
pub const MAX_FEE_BPS: u16 = 500;
pub const FEE_DENOMINATOR: u64 = 10_000;

/// Vault authority PDA seed: [VAULT_SEED, market_key].
pub const VAULT_SEED: &[u8] = b"vault";

/// A live order node's `seq` is its order id, which a cancel must quote back
/// so a stale cancel cannot kill a stranger's order that reused the slot.
/// `FREED_SEQ` (all ones) is skipped by the 64-bit order-id counter, so the
/// seq alone decides liveness without making stale-handle reuse practical.
pub const FREED_SEQ: u64 = u64::MAX;
