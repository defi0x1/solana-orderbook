//! Event logs for lifecycle instructions.
//!
//! Book state is recovered from account updates, not from events: the market
//! account is self-describing and carries a monotonic `seq`, so an indexer
//! diffs snapshots rather than replaying a log. What account updates cannot
//! announce is a market coming into existence, so the rare instructions that
//! create or reconfigure one log via `sol_log_data`. The hot path emits
//! nothing.

use crate::errors::ClobError;
use pinocchio::{
    account_info::AccountInfo,
    instruction::{AccountMeta, Instruction, Signer},
    log::sol_log_data,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    ProgramResult,
};

/// `TransferChecked` (SPL Token instruction index 12): `[from(w), mint(r),
/// to(w), authority(signer)]`, data `[12u8, amount: u64 LE, decimals: u8]`.
///
/// Built by hand and dispatched against whichever `token_program` account the
/// caller passes, because `pinocchio_token::instructions::TransferChecked`
/// hardcodes the legacy Token program id as the CPI target — this program
/// needs to serve vaults owned by either the legacy Token program or
/// Token-2022 from the same code path.
///
/// `TransferChecked` (not plain `Transfer`) is required, not just preferred:
/// the legacy unchecked `Transfer` instruction is rejected outright by
/// Token-2022 for any account carrying a `TransferFeeAmount`,
/// `TransferHookAccount`, or `PausableAccount` extension state (it returns
/// `MintRequiredForTransfer`, since those extensions need the mint account
/// to process). `TransferChecked` passes the mint through and correctly
/// applies a configured transfer fee; instructions that don't need the extra
/// account for a fee-free legacy-Token transfer still pay for it (a few
/// hundred CU), which is the cost of serving both programs uniformly.
pub(crate) struct CheckedTransfer<'a> {
    pub(crate) token_program: &'a AccountInfo,
    pub(crate) from: &'a AccountInfo,
    pub(crate) mint: &'a AccountInfo,
    pub(crate) to: &'a AccountInfo,
    pub(crate) authority: &'a AccountInfo,
    pub(crate) amount: u64,
    pub(crate) decimals: u8,
}

impl CheckedTransfer<'_> {
    #[inline(always)]
    pub(crate) fn invoke(&self) -> ProgramResult {
        self.invoke_signed(&[])
    }

    pub(crate) fn invoke_signed(&self, signers: &[Signer]) -> ProgramResult {
        let account_metas = [
            AccountMeta::writable(self.from.key()),
            AccountMeta::readonly(self.mint.key()),
            AccountMeta::writable(self.to.key()),
            AccountMeta::readonly_signer(self.authority.key()),
        ];
        let data = Self::encode(self.amount, self.decimals);

        let instruction = Instruction {
            program_id: self.token_program.key(),
            accounts: &account_metas,
            data: &data,
        };

        invoke_signed(
            &instruction,
            &[self.from, self.mint, self.to, self.authority],
            signers,
        )
    }

    /// `TransferChecked`'s instruction data: `[12u8, amount: u64 LE,
    /// decimals: u8]`. Split out from `invoke_signed` so the encoding is
    /// unit-testable without a live `AccountInfo`/CPI context.
    #[inline(always)]
    fn encode(amount: u64, decimals: u8) -> [u8; 10] {
        let mut data = [0u8; 10];
        data[0] = 12;
        data[1..9].copy_from_slice(&amount.to_le_bytes());
        data[9] = decimals;
        data
    }
}

/// Read a token account's `amount` field (u64 LE at byte offset 64). Both the
/// legacy SPL Token account and the Token-2022 account share this layout for
/// their first 165 bytes by design — extensions are appended after that, so
/// this read is valid regardless of which program owns the account or which
/// extensions (if any) it carries.
///
/// Deliberately excludes a `TransferFeeConfig` mint's withheld fees: under
/// that extension, the fee taken from an inbound transfer is parked in the
/// destination account's `TransferFeeAmount.withheld_amount` — separate TLV
/// state past this account's base 165 bytes, never folded into `amount`
/// here. That's exactly what this program needs: withheld fees are not
/// spendable by anyone until the mint's withdraw-withheld-authority harvests
/// them via a separate instruction that touches `withheld_amount` only, so
/// excluding them from every delta read/credit computed against a vault
/// keeps this program's accounting matched to what the vault can actually
/// move — the withheld authority accruing fees can never make a vault
/// insolvent from this program's point of view.
///
/// # Safety
///
/// `account_info`'s data must not be concurrently mutably borrowed. Length is
/// checked explicitly (not part of the safety contract) since a degenerate
/// vault account otherwise reads out of bounds instead of failing cleanly.
#[inline(always)]
pub(crate) unsafe fn token_amount(account_info: &AccountInfo) -> Result<u64, ProgramError> {
    if account_info.data_len() < 72 {
        return Err(ClobError::InvalidAccountLength.into());
    }
    let data = account_info.borrow_data_unchecked();
    Ok(u64::from_le_bytes([
        data[64], data[65], data[66], data[67], data[68], data[69], data[70], data[71],
    ]))
}

const CREATE_MARKET_DISCRIMINATOR: u8 = 0;
const CREATE_MARKET_EVENT_SIZE: usize = 115;

#[inline(always)]
pub(crate) fn log_create_market_event(
    market: &Pubkey,
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    tick_size: u64,
    base_lot_size: u64,
    fee_bps: u16,
) {
    let mut data = [0; CREATE_MARKET_EVENT_SIZE];
    data[0] = CREATE_MARKET_DISCRIMINATOR;
    data[1..33].copy_from_slice(market.as_ref());
    data[33..65].copy_from_slice(base_mint.as_ref());
    data[65..97].copy_from_slice(quote_mint.as_ref());
    data[97..105].copy_from_slice(&tick_size.to_le_bytes());
    data[105..113].copy_from_slice(&base_lot_size.to_le_bytes());
    data[113..115].copy_from_slice(&fee_bps.to_le_bytes());
    sol_log_data(&[&data]);
}

const UPDATE_FEE_DISCRIMINATOR: u8 = 2;
const UPDATE_FEE_EVENT_SIZE: usize = 35;

#[inline(always)]
pub(crate) fn log_update_fee_event(market: &Pubkey, fee_bps: u16) {
    let mut data = [0; UPDATE_FEE_EVENT_SIZE];
    data[0] = UPDATE_FEE_DISCRIMINATOR;
    data[1..33].copy_from_slice(market.as_ref());
    data[33..35].copy_from_slice(&fee_bps.to_le_bytes());
    sol_log_data(&[&data]);
}

const COLLECT_FEES_DISCRIMINATOR: u8 = 3;
const COLLECT_FEES_EVENT_SIZE: usize = 73;

#[inline(always)]
pub(crate) fn log_collect_fees_event(market: &Pubkey, authority: &Pubkey, amount: u64) {
    let mut data = [0; COLLECT_FEES_EVENT_SIZE];
    data[0] = COLLECT_FEES_DISCRIMINATOR;
    data[1..33].copy_from_slice(market.as_ref());
    data[33..65].copy_from_slice(authority.as_ref());
    data[65..73].copy_from_slice(&amount.to_le_bytes());
    sol_log_data(&[&data]);
}

const REANCHOR_DISCRIMINATOR: u8 = 4;
const REANCHOR_EVENT_SIZE: usize = 41;

#[inline(always)]
pub(crate) fn log_reanchor_event(market: &Pubkey, new_anchor: u32, evicted: u32) {
    let mut data = [0; REANCHOR_EVENT_SIZE];
    data[0] = REANCHOR_DISCRIMINATOR;
    data[1..33].copy_from_slice(market.as_ref());
    data[33..37].copy_from_slice(&new_anchor.to_le_bytes());
    data[37..41].copy_from_slice(&evicted.to_le_bytes());
    sol_log_data(&[&data]);
}

const CLAIM_SEAT_DISCRIMINATOR: u8 = 5;
const CLAIM_SEAT_EVENT_SIZE: usize = 67;

#[inline(always)]
pub(crate) fn log_claim_seat_event(market: &Pubkey, owner: &Pubkey, seat_index: u16) {
    let mut data = [0; CLAIM_SEAT_EVENT_SIZE];
    data[0] = CLAIM_SEAT_DISCRIMINATOR;
    data[1..33].copy_from_slice(market.as_ref());
    data[33..65].copy_from_slice(owner.as_ref());
    data[65..67].copy_from_slice(&seat_index.to_le_bytes());
    sol_log_data(&[&data]);
}

#[cfg(test)]
mod checked_transfer_tests {
    use super::CheckedTransfer;

    // TransferChecked (SPL Token instruction index 12) is accepted
    // byte-for-byte identically by the legacy Token program and Token-2022 —
    // this pins the encoding this program relies on for both.
    #[test]
    fn encodes_transfer_checked_layout() {
        let data = CheckedTransfer::encode(123_456_789_u64, 9);
        assert_eq!(data[0], 12, "TransferChecked discriminator");
        assert_eq!(
            u64::from_le_bytes(data[1..9].try_into().unwrap()),
            123_456_789_u64
        );
        assert_eq!(data[9], 9, "decimals");
    }

    #[test]
    fn encodes_zero_and_max_amount() {
        assert_eq!(
            u64::from_le_bytes(CheckedTransfer::encode(0, 0)[1..9].try_into().unwrap()),
            0
        );
        assert_eq!(
            u64::from_le_bytes(
                CheckedTransfer::encode(u64::MAX, 255)[1..9]
                    .try_into()
                    .unwrap()
            ),
            u64::MAX
        );
        assert_eq!(CheckedTransfer::encode(u64::MAX, 255)[9], 255);
    }
}
