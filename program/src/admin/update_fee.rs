use crate::errors::ClobError;
use crate::helpers::log_update_fee_event;
use crate::state::Market;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # UpdateFee
///
/// Change the taker fee. Authority-gated, capped at MAX_FEE_BPS.
///
/// Accounts:
///
/// 1. authority:   [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. fee_bps: u16,
///
/// Account Checks:
/// - Authority: must be a signer and match the authority stored in the header
/// - Market: owner, length and version via Market::check; must be writable
///
/// Event Data:
/// - discriminator: u8, (2u8)
/// - market: Pubkey,
/// - fee_bps: u16,
struct UpdateFeeAccounts<'a> {
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for UpdateFeeAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, market] = accounts else {
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

        Ok(Self { market })
    }
}

struct UpdateFeeInstructionData {
    fee_bps: u16,
}

impl<'a> TryFrom<&'a [u8]> for UpdateFeeInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 2 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let fee_bps = u16::from_le_bytes([data[0], data[1]]);

        Ok(Self { fee_bps })
    }
}

pub(crate) struct UpdateFee<'a> {
    accounts: UpdateFeeAccounts<'a>,
    instruction_data: UpdateFeeInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for UpdateFee<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("UpdateFee");

        let accounts = UpdateFeeAccounts::try_from(accounts)?;
        let instruction_data = UpdateFeeInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> UpdateFee<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 2;

    pub(crate) fn process(self) -> ProgramResult {
        let header = unsafe {
            Market::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };
        header.set_fee_bps(self.instruction_data.fee_bps)?;
        header.bump_seq();

        log_update_fee_event(self.accounts.market.key(), self.instruction_data.fee_bps);

        Ok(())
    }
}
