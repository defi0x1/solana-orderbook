use crate::errors::ClobError;
use crate::state::Config;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # UpdateConfig
///
/// Rotate the seat and market authorities.
///
/// Accounts:
///
/// 1. authority:   [signer]
/// 2. config:      [mut]
///
/// Parameters:
/// 1. seat_authority: Pubkey,
/// 2. market_authority: Pubkey,
///
/// Account Checks:
/// - Authority: must sign and match the config's own authority — the role that
///   owns the config, not the two it delegates
/// - Config: owner, length and version via Config::check; must be writable
struct UpdateConfigAccounts<'a> {
    config: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for UpdateConfigAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, config] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !authority.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !config.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Config::check(config)?;

        let cfg = unsafe { Config::from_bytes_unchecked(config.borrow_data_unchecked()) };
        if cfg.authority().ne(authority.key()) {
            return Err(ClobError::InvalidProgramAuthority.into());
        }

        Ok(Self { config })
    }
}

struct UpdateConfigInstructionData {
    seat_authority: [u8; 32],
    market_authority: [u8; 32],
}

impl<'a> TryFrom<&'a [u8]> for UpdateConfigInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 64 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let mut seat_authority = [0; 32];
        seat_authority.copy_from_slice(&data[..32]);
        let mut market_authority = [0; 32];
        market_authority.copy_from_slice(&data[32..]);
        Ok(Self {
            seat_authority,
            market_authority,
        })
    }
}

pub(crate) struct UpdateConfig<'a> {
    accounts: UpdateConfigAccounts<'a>,
    instruction_data: UpdateConfigInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for UpdateConfig<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("UpdateConfig");

        let accounts = UpdateConfigAccounts::try_from(accounts)?;
        let instruction_data = UpdateConfigInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> UpdateConfig<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 5;

    pub(crate) fn process(self) -> ProgramResult {
        // Neither delegated role may be bricked onto the zero address: no
        // signer can ever produce it, so this would permanently strand
        // ClaimSeat / Reanchor with no possible signer.
        if self.instruction_data.seat_authority == [0u8; 32]
            || self.instruction_data.market_authority == [0u8; 32]
        {
            return Err(ClobError::ZeroAuthority.into());
        }

        let cfg = unsafe {
            Config::from_bytes_unchecked_mut(self.accounts.config.borrow_mut_data_unchecked())
        };
        cfg.set_seat_authority(self.instruction_data.seat_authority);
        cfg.set_market_authority(self.instruction_data.market_authority);

        Ok(())
    }
}
