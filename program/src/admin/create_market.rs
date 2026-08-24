use crate::constants::*;
use crate::errors::ClobError;
use crate::helpers::log_create_market_event;
use crate::state::{BookRefMut, Config, Market};
use pinocchio::{
    account_info::AccountInfo, program_error::ProgramError, pubkey::find_program_address,
    ProgramResult,
};

/// # CreateMarket
///
/// Initialize a pre-created market account. The account itself is created
/// client-side in the same transaction (272 KB exceeds the 10,240-byte CPI
/// creation limit): `[SystemProgram::createAccount(space, owner = clob),
/// CreateMarket]`, with a throwaway keypair as the market address.
///
/// > Verify the account is program-owned, zeroed (version 0) and node-aligned
/// > Derive the vault authority PDA and verify both vaults are its ATAs
/// > Validate parameters and compute tick_limit (the u64-overflow guard)
/// > Initialize the header and thread the whole pool onto the free list
///
/// Market creation is permissionless.
///
/// Accounts:
///
/// 1. market:      [mut]
/// 2. base_mint:
/// 3. quote_mint:
/// 4. base_vault:
/// 5. quote_vault:
/// 6. authority:
/// 7. config:
///
/// Parameters:
/// 1. tick_size: u64,          // Quote atoms per base lot per tick
/// 2. base_lot_size: u64,      // Base atoms per lot
/// 3. anchor_tick: u32,        // Initial window start, multiple of 64
/// 4. fee_bps: u16,
/// 5. max_lots_per_order: u32,
///
/// Account Checks:
/// - Market: program-owned (assigned by the client-side createAccount),
///   version must be 0 (blocks re-initialization), pool region must divide
///   evenly into nodes — capacity is derived from the account length
/// - Vaults: must be the ATAs of the vault authority PDA for the two mints
///   (derived once here, ~4.5k CU — stored so the hot path never derives)
/// - Mints: owner must be the legacy Token program or Token-2022 — the flag
///   and decimals are recorded on the header and drive every later transfer's
///   routing and `TransferChecked` decimals field. A Token-2022 mint
///   carrying the `PermanentDelegate` extension is rejected outright (its
///   delegate could drain a vault directly, bypassing this program). Every
///   other extension is admitted — a market-creator-trust decision, same as
///   a low-quality legacy mint already is
/// - Authority: stored for UpdateFee/CollectFees; no signature required —
///   creating a market for someone else's authority only gifts them fee rights
/// - Config: owner, length and version via Config::check. Recorded on the
///   market, and from then on it decides who may take a seat here and who may
///   move the price window
///
/// Instruction Checks:
/// - tick_size, base_lot_size, max_lots_per_order must be non-zero
/// - anchor must be a multiple of 64 with the whole window under tick_limit
/// - tick_limit guarantees: tick * tick_size * lots * (10_000 + max fee) and
///   lots * base_lot_size never overflow u64 anywhere in the program
///
/// Event Data:
/// - discriminator: u8, (0u8)
/// - market: Pubkey,
/// - base_mint: Pubkey,
/// - quote_mint: Pubkey,
/// - tick_size: u64,
/// - base_lot_size: u64,
/// - fee_bps: u16,
struct CreateMarketAccounts<'a> {
    market: &'a AccountInfo,
    base_mint: &'a AccountInfo,
    quote_mint: &'a AccountInfo,
    base_vault: &'a AccountInfo,
    quote_vault: &'a AccountInfo,
    authority: &'a AccountInfo,
    config: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CreateMarketAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [market, base_mint, quote_mint, base_vault, quote_vault, authority, config] = accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        // The market is a caller-supplied keypair account rather than a PDA,
        // because its size exceeds what a program can allocate by CPI. Its
        // signature is therefore what proves the initializer is the party that
        // created the account, and not someone who found it uninitialized.
        if !market.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !market.is_owned_by(&crate::ID) {
            return Err(ClobError::InvalidAccountOwner.into());
        }
        // `% N != 0` rather than `.is_multiple_of(N)` on purpose: the SBF
        // build's platform-tools rustc (pinned well below 1.87) doesn't have
        // it, so the on-chain build requires this form even though a newer
        // host clippy flags it as the manual expansion.
        #[allow(clippy::manual_is_multiple_of)]
        let pool_region_misaligned = (market.data_len() - POOL_OFFSET) % NODE_LEN != 0;
        if market.data_len() < MIN_MARKET_LEN
            || pool_region_misaligned
            || ((market.data_len() - POOL_OFFSET) / NODE_LEN) as u32 > MAX_POOL_CAPACITY
        {
            return Err(ClobError::InvalidAccountLength.into());
        }
        {
            let header = unsafe { Market::from_bytes_unchecked(market.borrow_data_unchecked()) };
            if header.version() != 0 {
                return Err(ClobError::InvalidState.into());
            }
        }

        Config::check(config)?;

        Ok(Self {
            market,
            base_mint,
            quote_mint,
            base_vault,
            quote_vault,
            authority,
            config,
        })
    }
}

struct CreateMarketInstructionData {
    tick_size: u64,
    base_lot_size: u64,
    anchor_tick: u32,
    fee_bps: u16,
    max_lots_per_order: u32,
}

impl<'a> TryFrom<&'a [u8]> for CreateMarketInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 26 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let tick_size = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        let base_lot_size = u64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let anchor_tick = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let fee_bps = u16::from_le_bytes([data[20], data[21]]);
        let max_lots_per_order = u32::from_le_bytes([data[22], data[23], data[24], data[25]]);

        Ok(Self {
            tick_size,
            base_lot_size,
            anchor_tick,
            fee_bps,
            max_lots_per_order,
        })
    }
}

/// Resolve which token program owns a mint, rejecting anything but the
/// legacy Token program or Token-2022.
#[inline(always)]
fn mint_token_program_flag(mint: &AccountInfo) -> Result<u8, ProgramError> {
    if mint.is_owned_by(&pinocchio_token::ID) {
        Ok(TOKEN_PROGRAM_LEGACY)
    } else if mint.is_owned_by(&TOKEN_2022_ID) {
        Ok(TOKEN_PROGRAM_2022)
    } else {
        Err(ClobError::UnsupportedTokenProgram.into())
    }
}

/// Byte offset where TLV extension data begins for an extended Token-2022
/// Mint. NOT `82 + 1`: Token-2022 deliberately pads a Mint's extension
/// region so the `AccountType` marker sits at the same ABSOLUTE offset for
/// both Mints and Accounts — byte 165 (`Account::LEN`), not byte 82
/// (`Mint::LEN`) — so the byte range `[82, 165)` is 83 bytes of REQUIRED
/// zero padding, and TLV entries only start at byte 166. Verified against
/// `spl_token_2022::extension::type_and_tlv_indices` in the installed
/// `spl-token-2022` crate source (`~/.cargo/registry/.../spl-token-2022-*/
/// src/extension/mod.rs`): for a Mint, that function computes
/// `account_type_index = Account::LEN(165) - Mint::SIZE_OF(82) = 83`
/// relative to `rest_input` (which itself starts at absolute byte 82, i.e.
/// after the base struct) — so the marker is at absolute `82 + 83 = 165`
/// and TLV starts at absolute `165 + 1 = 166`. An earlier version of this
/// constant used `83` directly as an absolute offset, which is actually
/// that *relative* value — landing inside the mandatory zero padding and
/// desyncing the scan once it crossed into real TLV data, a false-negative
/// risk on the one check this exists to make reliable. A legacy-Token mint
/// is never extended and stops at exactly 82 bytes; a Token-2022 mint with
/// zero extensions is also exactly 82 bytes (the marker and TLV region only
/// exist once at least one extension is present).
const MINT_TLV_OFFSET: usize = 166;

/// `ExtensionType::PermanentDelegate`'s discriminant in the Token-2022
/// interface's `#[repr(u16)]` `ExtensionType` enum — verified against
/// `solana-program/token-2022`'s published `spl-token-2022` crate
/// (`ExtensionType::PermanentDelegate = 12`), not hand-derived. TLV entries
/// are `[type: u16 LE][length: u16 LE][value]`.
const PERMANENT_DELEGATE_EXTENSION_TYPE: u16 = 12;

/// Read `decimals` (u8 at byte offset 44) from a mint account and confirm
/// it's actually initialized (`is_initialized` at byte offset 45), both
/// inside the first-82-byte layout the legacy SPL Token `Mint` and the
/// Token-2022 `Mint` share by design. Length is exact for a legacy mint
/// (always precisely 82 bytes) and a lower bound for Token-2022 (extensions
/// append data past 82) — conflating the two would let a legacy mint claim
/// to carry extensions it structurally cannot have.
#[inline(always)]
fn mint_decimals(mint: &AccountInfo, token_program: u8) -> Result<u8, ProgramError> {
    let len = mint.data_len();
    let len_ok = if token_program == TOKEN_PROGRAM_2022 {
        len >= 82
    } else {
        len == 82
    };
    if !len_ok {
        return Err(ClobError::InvalidAccountLength.into());
    }
    let data = unsafe { mint.borrow_data_unchecked() };
    if data[45] == 0 {
        return Err(ClobError::InvalidState.into());
    }
    // An extended Token-2022 account (len > 82) carries an `AccountType`
    // marker at absolute byte 165 (see the derivation on `MINT_TLV_OFFSET`
    // below) — `Mint` and `Account` share that exact position by design, so
    // without this check a genuine Token-2022 *token account* would pass
    // every prior test here (owner is Token-2022, `is_initialized`-shaped
    // byte 45 is frequently nonzero garbage from its `owner` pubkey field)
    // and be recorded as this market's mint. Not a fund risk — the vault ATA
    // derived from a non-mint address can never be created, and every
    // TransferChecked would then fail its own decimals check — but a bogus
    // `decimals` value is worth rejecting outright rather than only failing
    // later in a confusing place.
    if token_program == TOKEN_PROGRAM_2022 && len > 82 {
        const MINT_ACCOUNT_TYPE_OFFSET: usize = MINT_TLV_OFFSET - 1;
        if len <= MINT_ACCOUNT_TYPE_OFFSET || data[MINT_ACCOUNT_TYPE_OFFSET] != 1 {
            return Err(ClobError::InvalidState.into());
        }
    }
    Ok(data[44])
}

/// Scan a Token-2022 mint's TLV extension region for `PermanentDelegate` and
/// reject it if present — see the doc comment on `CreateMarket` for why.
/// Every other extension (transfer fees, hooks, pausable, confidential
/// transfer, ...) is left admitted: this is a single targeted check, not an
/// extension allow-list. A legacy-Token mint or an extension-free Token-2022
/// mint is exactly 82 bytes and returns `false` immediately. Malformed or
/// adversarial TLV data (e.g. an oversized `length`) can only make the scan
/// stop early — every byte read here is guarded by the loop bound, so
/// there's no out-of-bounds access regardless of what the `length` field
/// claims.
fn has_permanent_delegate(mint: &AccountInfo) -> bool {
    let data = unsafe { mint.borrow_data_unchecked() };
    has_permanent_delegate_bytes(data)
}

/// Byte-level scan, split out from `has_permanent_delegate` so the TLV
/// offset/stride logic is directly unit-testable against a hand-built
/// buffer without needing a live `AccountInfo`.
fn has_permanent_delegate_bytes(data: &[u8]) -> bool {
    let len = data.len();
    if len <= 82 {
        return false;
    }
    let mut i = MINT_TLV_OFFSET;
    while i + 4 <= len {
        let ext_type = u16::from_le_bytes([data[i], data[i + 1]]);
        if ext_type == PERMANENT_DELEGATE_EXTENSION_TYPE {
            return true;
        }
        let ext_len = u16::from_le_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 4 + ext_len;
    }
    false
}

pub(crate) struct CreateMarket<'a> {
    accounts: CreateMarketAccounts<'a>,
    instruction_data: CreateMarketInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CreateMarket<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CreateMarket");

        let accounts = CreateMarketAccounts::try_from(accounts)?;
        let instruction_data = CreateMarketInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> CreateMarket<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 0;

    pub(crate) fn process(self) -> ProgramResult {
        let d = &self.instruction_data;

        // Parameter validation + the u64-overflow guard.
        if d.tick_size == 0 || d.base_lot_size == 0 || d.max_lots_per_order == 0 {
            return Err(ClobError::InvalidMarketParams.into());
        }
        // The anchor moves in whole bitmap words, and tick 0 is not a
        // tradeable price. `% 64 != 0` rather than `!is_multiple_of(64)` for
        // the same SBF-toolchain MSRV reason as above.
        #[allow(clippy::manual_is_multiple_of)]
        let anchor_misaligned = d.anchor_tick % 64 != 0;
        if anchor_misaligned || d.anchor_tick == 0 {
            return Err(ClobError::InvalidMarketParams.into());
        }
        // Both vaults derive from the mint, so a self-paired market would keep
        // two ledgers over one vault.
        if self.accounts.base_mint.key() == self.accounts.quote_mint.key() {
            return Err(ClobError::InvalidMarketParams.into());
        }
        // Coherence: one lot must be worth at least one quote atom at the
        // cheapest tick in the window, otherwise a whole level of resting size
        // rounds to nothing and trades for free.
        if d.tick_size.checked_mul(d.anchor_tick as u64).is_none() {
            return Err(ClobError::InvalidMarketParams.into());
        }
        // Fee-inclusive worst case: notional * (10_000 + MAX_FEE_BPS).
        let denom = d
            .tick_size
            .checked_mul(d.max_lots_per_order as u64)
            .and_then(|x| x.checked_mul(FEE_DENOMINATOR + MAX_FEE_BPS as u64))
            .ok_or(ClobError::InvalidMarketParams)?;
        d.base_lot_size
            .checked_mul(d.max_lots_per_order as u64)
            .ok_or(ClobError::InvalidMarketParams)?;
        let tick_limit = core::cmp::min(
            u64::MAX / denom,
            (u32::MAX as usize - 2 * WINDOW_TICKS) as u64,
        ) as u32;
        if d.anchor_tick.checked_add(WINDOW_MASK).is_none()
            || d.anchor_tick + WINDOW_MASK > tick_limit
        {
            return Err(ClobError::InvalidMarketParams.into());
        }

        // Each mint's owner must be the legacy Token program or Token-2022 —
        // anything else has no defined transfer/account layout this program
        // can settle against. This is also what the vault ATA derivation
        // below needs: the two programs derive different ATA addresses for
        // the same (wallet, mint) pair.
        let base_program_flag = mint_token_program_flag(self.accounts.base_mint)?;
        let quote_program_flag = mint_token_program_flag(self.accounts.quote_mint)?;
        let base_program_id = token_program_id(base_program_flag);
        let quote_program_id = token_program_id(quote_program_flag);

        // Mint's `decimals` (u8 at byte offset 44) sits inside the first 82
        // bytes both programs share by design; TransferChecked needs it on
        // every transfer, so it's captured once here instead of re-parsing
        // the mint account on every Deposit/Withdraw/Swap.
        let base_decimals = mint_decimals(self.accounts.base_mint, base_program_flag)?;
        let quote_decimals = mint_decimals(self.accounts.quote_mint, quote_program_flag)?;

        // A PermanentDelegate-carrying mint lets its designated delegate
        // move tokens out of ANY holder's account for that mint — including
        // this program's own vaults — with no involvement from this program
        // at all. Every fill/cancel/withdraw here assumes a vault's balance
        // only ever changes through this program's own CPIs; a permanent
        // delegate breaks that assumption completely, reopening the same
        // insolvency class Token-2022 support otherwise closes, through a
        // door this program cannot see or gate. Every other extension is a
        // market-creator-trust decision, same as a low-quality legacy mint
        // already is — this is the one specific case that isn't.
        if base_program_flag == TOKEN_PROGRAM_2022
            && has_permanent_delegate(self.accounts.base_mint)
        {
            return Err(ClobError::PermanentDelegateNotAllowed.into());
        }
        if quote_program_flag == TOKEN_PROGRAM_2022
            && has_permanent_delegate(self.accounts.quote_mint)
        {
            return Err(ClobError::PermanentDelegateNotAllowed.into());
        }

        // Derive the vault authority PDA and verify the vault ATAs. This is
        // the only place in the program that derives anything.
        let (vault_authority, bump) =
            find_program_address(&[VAULT_SEED, self.accounts.market.key()], &crate::ID);
        let (expected_base_vault, _) = find_program_address(
            &[
                vault_authority.as_ref(),
                base_program_id.as_ref(),
                self.accounts.base_mint.key().as_ref(),
            ],
            &pinocchio_associated_token_account::ID,
        );
        if self.accounts.base_vault.key().ne(&expected_base_vault) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        let (expected_quote_vault, _) = find_program_address(
            &[
                vault_authority.as_ref(),
                quote_program_id.as_ref(),
                self.accounts.quote_mint.key().as_ref(),
            ],
            &pinocchio_associated_token_account::ID,
        );
        if self.accounts.quote_vault.key().ne(&expected_quote_vault) {
            return Err(ClobError::InvalidTokenAddress.into());
        }

        // Initialize the header and thread the free list.
        let capacity = ((self.accounts.market.data_len() - POOL_OFFSET) / NODE_LEN) as u32;
        // `BookRefMut` sizes its pool slice from the recorded capacity, so the
        // header has to carry the right value before the view is carved.
        unsafe {
            Market::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
                .set_order_pool_capacity(capacity);
        }
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        book.header.set_version(MARKET_VERSION);
        book.header.set_bump([bump]);
        book.header.set_base_mint(*self.accounts.base_mint.key());
        book.header.set_quote_mint(*self.accounts.quote_mint.key());
        book.header.set_base_vault(*self.accounts.base_vault.key());
        book.header
            .set_quote_vault(*self.accounts.quote_vault.key());
        book.header.set_vault_authority(vault_authority);
        book.header.set_authority(*self.accounts.authority.key());
        book.header.set_config(*self.accounts.config.key());
        book.header.set_tick_size(d.tick_size);
        book.header.set_base_lot_size(d.base_lot_size);
        book.header.set_anchor_tick(d.anchor_tick);
        book.header.set_tick_limit(tick_limit);
        book.header.set_fee_bps(d.fee_bps)?;
        book.header.set_max_lots_per_order(d.max_lots_per_order);
        book.header.set_base_token_program(base_program_flag);
        book.header.set_quote_token_program(quote_program_flag);
        book.header.set_base_decimals(base_decimals);
        book.header.set_quote_decimals(quote_decimals);
        book.header.set_free_head(NIL);
        // Node 0 is the reserved NIL node: the free list starts at 1, so no
        // live order can ever occupy index 0 and a zeroed link always means
        // "none".
        book.thread_free_list(1, capacity);
        book.header.bump_seq();

        log_create_market_event(
            self.accounts.market.key(),
            self.accounts.base_mint.key(),
            self.accounts.quote_mint.key(),
            d.tick_size,
            d.base_lot_size,
            d.fee_bps,
        );

        Ok(())
    }
}

#[cfg(test)]
mod permanent_delegate_tests {
    use super::{has_permanent_delegate_bytes, MINT_TLV_OFFSET, PERMANENT_DELEGATE_EXTENSION_TYPE};

    // A minimal but structurally real Token-2022 extended-Mint buffer: 82
    // bytes of base Mint (zeroed — irrelevant to the scan), 83 bytes of
    // mandatory zero padding out to Account::LEN (165), a 1-byte AccountType
    // marker (166 total so far), then one TLV entry.
    fn buffer_with_one_extension(ext_type: u16, value_len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; MINT_TLV_OFFSET + 4 + value_len];
        buf[MINT_TLV_OFFSET] = (ext_type & 0xFF) as u8;
        buf[MINT_TLV_OFFSET + 1] = (ext_type >> 8) as u8;
        buf[MINT_TLV_OFFSET + 2] = (value_len & 0xFF) as u8;
        buf[MINT_TLV_OFFSET + 3] = (value_len >> 8) as u8;
        buf
    }

    #[test]
    fn tlv_offset_is_166_not_83() {
        // Pinned directly: 82-byte Mint base + 83 bytes of padding to
        // Account::LEN(165) + 1-byte AccountType marker = 166. See the doc
        // comment on `MINT_TLV_OFFSET` for the full derivation and source
        // citation.
        assert_eq!(MINT_TLV_OFFSET, 166);
    }

    #[test]
    fn finds_permanent_delegate_as_the_only_extension() {
        let buf = buffer_with_one_extension(PERMANENT_DELEGATE_EXTENSION_TYPE, 32);
        assert!(has_permanent_delegate_bytes(&buf));
    }

    #[test]
    fn finds_permanent_delegate_after_an_earlier_extension() {
        // TransferFeeConfig-shaped (110-byte value) entry first, then
        // PermanentDelegate — proves the stride correctly walks past a
        // preceding extension using its own `length` field rather than only
        // working when the target happens to be first.
        let mut buf = buffer_with_one_extension(1 /* TransferFeeConfig */, 110);
        let pd_start = buf.len();
        buf.extend(
            buffer_with_one_extension(PERMANENT_DELEGATE_EXTENSION_TYPE, 32)[MINT_TLV_OFFSET..]
                .iter(),
        );
        assert!(has_permanent_delegate_bytes(&buf), "pd_start={pd_start}");
    }

    #[test]
    fn does_not_false_positive_on_an_unrelated_extension() {
        let buf = buffer_with_one_extension(1 /* TransferFeeConfig */, 110);
        assert!(!has_permanent_delegate_bytes(&buf));
    }

    #[test]
    fn does_not_false_positive_on_a_bare_82_byte_mint() {
        let buf = vec![0u8; 82];
        assert!(!has_permanent_delegate_bytes(&buf));
    }

    #[test]
    fn a_82_byte_token_2022_mint_with_no_extensions_is_not_flagged() {
        // Token-2022 permits an extension-free Mint to stay exactly 82
        // bytes (no marker, no TLV region at all) — must not scan into
        // nonexistent data.
        let buf = vec![0u8; 82];
        assert!(!has_permanent_delegate_bytes(&buf));
    }

    #[test]
    fn oversized_length_field_terminates_the_scan_without_panicking() {
        // An adversarial/malformed `length` that would walk `i` past the
        // buffer must stop the loop, not panic or infinite-loop.
        let buf = buffer_with_one_extension(0xBEEF, 0xFFFF);
        assert!(!has_permanent_delegate_bytes(&buf));
    }
}
