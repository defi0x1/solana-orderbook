use crate::constants::VAULT_SEED;
use crate::errors::ClobError;
use crate::helpers::{log_collect_fees_event, CheckedTransfer};
use crate::state::Market;
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};

/// # CollectFees
///
/// Transfer the accrued taker fees (quote atoms) from the quote vault to the
/// authority's token account and zero the accrual counter.
///
/// Accounts:
///
/// 1. authority:           [signer]
/// 2. market:               [mut]
/// 3. quote_vault:           [mut]
/// 4. authority_quote_ata:  [mut]
/// 5. vault_authority:
/// 6. quote_mint:
/// 7. quote_token_program:  [executable]
///
/// Parameters: none
///
/// Account Checks:
/// - Authority: must be a signer and match the authority stored in the header
/// - Market: owner, length and version via Market::check; must be writable
/// - Quote vault + vault_authority: compared against the header
/// - Quote token program: compared against the header's recorded flag
/// - Authority ATA / mint: no need to check beyond what TransferChecked
///   itself enforces
///
/// Event Data:
/// - discriminator: u8, (3u8)
/// - market: Pubkey,
/// - authority: Pubkey,
/// - amount: u64,
struct CollectFeesAccounts<'a> {
    authority: &'a AccountInfo,
    market: &'a AccountInfo,
    quote_vault: &'a AccountInfo,
    authority_quote_ata: &'a AccountInfo,
    vault_authority: &'a AccountInfo,
    quote_mint: &'a AccountInfo,
    quote_token_program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CollectFeesAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, market, quote_vault, authority_quote_ata, vault_authority, quote_mint, quote_token_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !authority.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Market::check(market)?;

        let header = unsafe { Market::from_bytes_unchecked(market.borrow_data_unchecked()) };
        if header.authority().ne(authority.key()) {
            return Err(ClobError::InvalidProgramAuthority.into());
        }
        if quote_vault.key().ne(header.quote_vault()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if vault_authority.key().ne(header.vault_authority()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if quote_token_program
            .key()
            .ne(&header.quote_token_program_id())
        {
            return Err(ClobError::UnsupportedTokenProgram.into());
        }

        Ok(Self {
            authority,
            market,
            quote_vault,
            authority_quote_ata,
            vault_authority,
            quote_mint,
            quote_token_program,
        })
    }
}

pub(crate) struct CollectFees<'a> {
    accounts: CollectFeesAccounts<'a>,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CollectFees<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CollectFees");

        let accounts = CollectFeesAccounts::try_from(accounts)?;

        Ok(Self { accounts })
    }
}

impl<'a> CollectFees<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 3;

    pub(crate) fn process(self) -> ProgramResult {
        let (amount, bump, quote_decimals) = {
            let header = unsafe {
                Market::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
            };
            let amount = header.take_fees_accrued();
            header.bump_seq();
            (amount, *header.bump(), header.quote_decimals())
        };

        if amount > 0 {
            let market_key = self.accounts.market.key();
            let seeds = [
                Seed::from(VAULT_SEED),
                Seed::from(market_key.as_ref()),
                Seed::from(bump.as_ref()),
            ];

            CheckedTransfer {
                token_program: self.accounts.quote_token_program,
                from: self.accounts.quote_vault,
                mint: self.accounts.quote_mint,
                to: self.accounts.authority_quote_ata,
                authority: self.accounts.vault_authority,
                amount,
                decimals: quote_decimals,
            }
            .invoke_signed(&[Signer::from(&seeds)])?;
        }

        log_collect_fees_event(
            self.accounts.market.key(),
            self.accounts.authority.key(),
            amount,
        );

        Ok(())
    }
}
