use crate::errors::ClobError;
use crate::helpers::{token_amount, CheckedTransfer};
use crate::state::{BookRefMut, Market};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Deposit
///
/// Fund a seat's internal balances. Cold path: after this, every fill, cancel
/// and refund is pure memory arithmetic — no CPI until Withdraw.
///
/// > Verify the seat belongs to the signer and the vaults match the header
/// > Transfer the base leg (if non-zero), then the quote leg (if non-zero)
/// > Credit the seat's free balances by the OBSERVED vault delta, not the
/// >   instruction-stated amount
///
/// The delta-based crediting is the load-bearing part: a Token-2022 mint with
/// a transfer-fee (or any other amount-reducing) extension delivers less to
/// the vault than `base_atoms`/`quote_atoms` requests. Crediting the
/// instruction-stated amount would over-credit the seat relative to what the
/// vault actually holds — a slow-drip insolvency across every such deposit.
/// Reading the vault's own `amount` field before and after each transfer and
/// crediting the difference is correct regardless of mint or extension, and
/// costs one extra account read per non-zero leg.
///
/// Accounts:
///
/// 1. owner:                [signer]
/// 2. market:                [mut]
/// 3. owner_base_ata:        [mut]
/// 4. owner_quote_ata:       [mut]
/// 5. base_vault:             [mut]
/// 6. quote_vault:            [mut]
/// 7. base_mint:
/// 8. quote_mint:
/// 9. base_token_program:    [executable]
/// 10. quote_token_program:  [executable]
///
/// Parameters:
/// 1. seat_index: u16,
/// 2. base_atoms: u64,
/// 3. quote_atoms: u64,
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner (the transfers would
///   also fail, but the seat check must not be skippable)
/// - Market: owner, length and version via Market::check; must be writable
/// - Vaults: compared against the addresses stored in the header
/// - Owner ATAs: no need to check — the transfers fail on wrong mint or owner
/// - Mints: no need to check beyond what TransferChecked itself enforces —
///   the header's stored decimals came from these mints at CreateMarket
/// - Token programs: each compared against the header's recorded flag for
///   that asset — with two valid programs in play, a wrong one would
///   otherwise just fail the CPI, but failing on a clear check beats failing
///   inside someone else's program
///
/// Instruction Checks:
/// - Amounts: no need to check — the transfers fail if underfunded
///
/// Event Data: none — deposits are replayed from instruction data.
struct DepositAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
    owner_base_ata: &'a AccountInfo,
    owner_quote_ata: &'a AccountInfo,
    base_vault: &'a AccountInfo,
    quote_vault: &'a AccountInfo,
    base_mint: &'a AccountInfo,
    quote_mint: &'a AccountInfo,
    base_token_program: &'a AccountInfo,
    quote_token_program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for DepositAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [owner, market, owner_base_ata, owner_quote_ata, base_vault, quote_vault, base_mint, quote_mint, base_token_program, quote_token_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !owner.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Market::check(market)?;

        let header = unsafe { Market::from_bytes_unchecked(market.borrow_data_unchecked()) };
        if base_vault.key().ne(header.base_vault()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if quote_vault.key().ne(header.quote_vault()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if base_token_program.key().ne(&header.base_token_program_id()) {
            return Err(ClobError::UnsupportedTokenProgram.into());
        }
        if quote_token_program
            .key()
            .ne(&header.quote_token_program_id())
        {
            return Err(ClobError::UnsupportedTokenProgram.into());
        }

        Ok(Self {
            owner,
            market,
            owner_base_ata,
            owner_quote_ata,
            base_vault,
            quote_vault,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
        })
    }
}

struct DepositInstructionData {
    seat_index: u16,
    base_atoms: u64,
    quote_atoms: u64,
}

impl<'a> TryFrom<&'a [u8]> for DepositInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 18 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);
        let base_atoms = u64::from_le_bytes([
            data[2], data[3], data[4], data[5], data[6], data[7], data[8], data[9],
        ]);
        let quote_atoms = u64::from_le_bytes([
            data[10], data[11], data[12], data[13], data[14], data[15], data[16], data[17],
        ]);

        Ok(Self {
            seat_index,
            base_atoms,
            quote_atoms,
        })
    }
}

pub(crate) struct Deposit<'a> {
    accounts: DepositAccounts<'a>,
    instruction_data: DepositInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Deposit<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Deposit");

        let accounts = DepositAccounts::try_from(accounts)?;
        let instruction_data = DepositInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Deposit<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 12;

    pub(crate) fn process(self) -> ProgramResult {
        // This `&mut` stays alive across the transfer CPIs below — see the
        // matching comment in `Swap::process` for why that's safe (`Market::
        // check` already requires `market` be owned by this program, so it
        // can never validly appear as a token-side account in the same CPI).
        let book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;
        let seat_index = self.instruction_data.seat_index;

        // Pull each non-zero leg, then credit the seat by what the vault
        // actually received — not by the requested amount. Transferring
        // before crediting (rather than the reverse, as before) is what
        // makes the observed delta available to credit against.
        if self.instruction_data.base_atoms > 0 {
            let pre = unsafe { token_amount(self.accounts.base_vault)? };
            CheckedTransfer {
                token_program: self.accounts.base_token_program,
                from: self.accounts.owner_base_ata,
                mint: self.accounts.base_mint,
                to: self.accounts.base_vault,
                authority: self.accounts.owner,
                amount: self.instruction_data.base_atoms,
                decimals: book.header.base_decimals(),
            }
            .invoke()?;
            let post = unsafe { token_amount(self.accounts.base_vault)? };
            let delta = post.checked_sub(pre).ok_or(ClobError::InvalidState)?;
            let seat = &mut book.seats[seat_index as usize];
            seat.set_base_free(
                seat.base_free()
                    .checked_add(delta)
                    .ok_or(ProgramError::ArithmeticOverflow)?,
            );
        }
        if self.instruction_data.quote_atoms > 0 {
            let pre = unsafe { token_amount(self.accounts.quote_vault)? };
            CheckedTransfer {
                token_program: self.accounts.quote_token_program,
                from: self.accounts.owner_quote_ata,
                mint: self.accounts.quote_mint,
                to: self.accounts.quote_vault,
                authority: self.accounts.owner,
                amount: self.instruction_data.quote_atoms,
                decimals: book.header.quote_decimals(),
            }
            .invoke()?;
            let post = unsafe { token_amount(self.accounts.quote_vault)? };
            let delta = post.checked_sub(pre).ok_or(ClobError::InvalidState)?;
            let seat = &mut book.seats[seat_index as usize];
            seat.set_quote_free(
                seat.quote_free()
                    .checked_add(delta)
                    .ok_or(ProgramError::ArithmeticOverflow)?,
            );
        }

        book.header.bump_seq();
        Ok(())
    }
}
