use crate::constants::VAULT_SEED;
use crate::errors::ClobError;
use crate::helpers::CheckedTransfer;
use crate::state::{BookRefMut, Market};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};

/// # Withdraw
///
/// Move free balances from the seat back to the owner's token accounts, signed
/// by the vault authority PDA. `u64::MAX` on either amount withdraws the whole
/// free balance of that asset.
///
/// No delta accounting here (unlike Deposit): the vault is the *source*, so a
/// transfer debits it by exactly the requested amount regardless of any
/// destination-side extension on the recipient's own account — a Token-2022
/// transfer fee, if any, is taken from what the owner's ATA receives, not
/// from the vault. That's expected Token-2022 behavior, not a solvency risk
/// here, so the seat is debited by the requested amount as before.
///
/// Accounts:
///
/// 1. owner:                 [signer]
/// 2. market:                 [mut]
/// 3. owner_base_ata:         [mut]
/// 4. owner_quote_ata:        [mut]
/// 5. base_vault:              [mut]
/// 6. quote_vault:             [mut]
/// 7. vault_authority:
/// 8. base_mint:
/// 9. quote_mint:
/// 10. base_token_program:    [executable]
/// 11. quote_token_program:   [executable]
///
/// Parameters:
/// 1. seat_index: u16,
/// 2. base_atoms: u64,     // u64::MAX = all free base
/// 3. quote_atoms: u64,    // u64::MAX = all free quote
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner — this check IS the
///   authorization for taking funds out
/// - Market: owner, length and version via Market::check; must be writable
/// - Vaults + vault_authority: compared against the addresses in the header
/// - Owner ATAs: no need to check — the transfers fail on wrong mint
/// - Mints: no need to check beyond what TransferChecked itself enforces
/// - Token programs: each compared against the header's recorded flag for
///   that asset
///
/// Instruction Checks:
/// - Amounts above the free balance -> InsufficientBalance
///
/// Event Data: none — withdrawals are replayed from instruction data.
struct WithdrawAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
    owner_base_ata: &'a AccountInfo,
    owner_quote_ata: &'a AccountInfo,
    base_vault: &'a AccountInfo,
    quote_vault: &'a AccountInfo,
    vault_authority: &'a AccountInfo,
    base_mint: &'a AccountInfo,
    quote_mint: &'a AccountInfo,
    base_token_program: &'a AccountInfo,
    quote_token_program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for WithdrawAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [owner, market, owner_base_ata, owner_quote_ata, base_vault, quote_vault, vault_authority, base_mint, quote_mint, base_token_program, quote_token_program] =
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
        if vault_authority.key().ne(header.vault_authority()) {
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
            vault_authority,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
        })
    }
}

struct WithdrawInstructionData {
    seat_index: u16,
    base_atoms: u64,
    quote_atoms: u64,
}

impl<'a> TryFrom<&'a [u8]> for WithdrawInstructionData {
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

pub(crate) struct Withdraw<'a> {
    accounts: WithdrawAccounts<'a>,
    instruction_data: WithdrawInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Withdraw<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Withdraw");

        let accounts = WithdrawAccounts::try_from(accounts)?;
        let instruction_data = WithdrawInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Withdraw<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 13;

    pub(crate) fn process(self) -> ProgramResult {
        let (base_out, quote_out, bump, base_decimals, quote_decimals) = {
            let book = unsafe {
                BookRefMut::from_bytes_unchecked_mut(
                    self.accounts.market.borrow_mut_data_unchecked(),
                )
            };

            // Authorization: the seat must belong to the signer
            book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;

            let seat = &mut book.seats[self.instruction_data.seat_index as usize];

            let base_out = if self.instruction_data.base_atoms == u64::MAX {
                seat.base_free()
            } else {
                self.instruction_data.base_atoms
            };
            let quote_out = if self.instruction_data.quote_atoms == u64::MAX {
                seat.quote_free()
            } else {
                self.instruction_data.quote_atoms
            };

            if seat.base_free() < base_out || seat.quote_free() < quote_out {
                return Err(ClobError::InsufficientBalance.into());
            }
            seat.set_base_free(seat.base_free() - base_out);
            seat.set_quote_free(seat.quote_free() - quote_out);

            book.header.bump_seq();
            (
                base_out,
                quote_out,
                *book.header.bump(),
                book.header.base_decimals(),
                book.header.quote_decimals(),
            )
        };

        let market_key = self.accounts.market.key();
        let seeds = [
            Seed::from(VAULT_SEED),
            Seed::from(market_key.as_ref()),
            Seed::from(bump.as_ref()),
        ];

        if base_out > 0 {
            CheckedTransfer {
                token_program: self.accounts.base_token_program,
                from: self.accounts.base_vault,
                mint: self.accounts.base_mint,
                to: self.accounts.owner_base_ata,
                authority: self.accounts.vault_authority,
                amount: base_out,
                decimals: base_decimals,
            }
            .invoke_signed(&[Signer::from(&seeds)])?;
        }
        if quote_out > 0 {
            CheckedTransfer {
                token_program: self.accounts.quote_token_program,
                from: self.accounts.quote_vault,
                mint: self.accounts.quote_mint,
                to: self.accounts.owner_quote_ata,
                authority: self.accounts.vault_authority,
                amount: quote_out,
                decimals: quote_decimals,
            }
            .invoke_signed(&[Signer::from(&seeds)])?;
        }

        Ok(())
    }
}
