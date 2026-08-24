use crate::constants::{CONFIG_LEN, CONFIG_SEED, CONFIG_VERSION};
use crate::errors::ClobError;
use crate::state::Config;
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    pubkey::find_program_address,
    sysvars::{rent::Rent, Sysvar},
    ProgramResult,
};
use pinocchio_system::instructions::CreateAccount;

/// # CreateConfig
///
/// Create the permission set a venue's markets run under. Anyone may create a
/// config; it is a PDA of its own authority, so creating one grants nothing
/// except over markets that later name it.
///
/// > Derive the config PDA from the authority and verify the passed account
/// > Allocate it and write the three roles
///
/// Accounts:
///
/// 1. payer:           [signer, mut]
/// 2. config:          [mut]
/// 3. authority:       [signer]
/// 4. system_program:  [executable]
///
/// Parameters:
/// 1. seat_authority: Pubkey,      // May sign ClaimSeat
/// 2. market_authority: Pubkey,    // May sign Reanchor
///
/// Account Checks:
/// - Payer: must sign — it funds the rent
/// - Config: must be the PDA of `authority`, which makes the address itself
///   proof of who owns it, and makes a second config for the same authority
///   impossible
/// - Authority: must sign, so a config cannot be created naming someone else
/// - System Program: no need to check — the allocation fails if it is wrong
struct CreateConfigAccounts<'a> {
    payer: &'a AccountInfo,
    config: &'a AccountInfo,
    authority: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CreateConfigAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [payer, config, authority, _system_program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !payer.is_signer() || !authority.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !config.is_writable() {
            return Err(ClobError::NotMutable.into());
        }

        Ok(Self {
            payer,
            config,
            authority,
        })
    }
}

struct CreateConfigInstructionData {
    seat_authority: [u8; 32],
    market_authority: [u8; 32],
}

impl<'a> TryFrom<&'a [u8]> for CreateConfigInstructionData {
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

pub(crate) struct CreateConfig<'a> {
    accounts: CreateConfigAccounts<'a>,
    instruction_data: CreateConfigInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CreateConfig<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CreateConfig");

        let accounts = CreateConfigAccounts::try_from(accounts)?;
        let instruction_data = CreateConfigInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> CreateConfig<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 4;

    pub(crate) fn process(self) -> ProgramResult {
        // Neither delegated role may be bricked onto the zero address: no
        // signer can ever produce it, so this would permanently strand
        // ClaimSeat / Reanchor with no possible signer.
        if self.instruction_data.seat_authority == [0u8; 32]
            || self.instruction_data.market_authority == [0u8; 32]
        {
            return Err(ClobError::ZeroAuthority.into());
        }

        let (expected, bump) =
            find_program_address(&[CONFIG_SEED, self.accounts.authority.key()], &crate::ID);
        if self.accounts.config.key().ne(&expected) {
            return Err(ClobError::InvalidAccountOwner.into());
        }

        let bump_seed = [bump];
        let seeds = [
            Seed::from(CONFIG_SEED),
            Seed::from(self.accounts.authority.key().as_ref()),
            Seed::from(bump_seed.as_ref()),
        ];

        CreateAccount {
            from: self.accounts.payer,
            to: self.accounts.config,
            lamports: Rent::get()?.minimum_balance(CONFIG_LEN),
            space: CONFIG_LEN as u64,
            owner: &crate::ID,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        let cfg = unsafe {
            Config::from_bytes_unchecked_mut(self.accounts.config.borrow_mut_data_unchecked())
        };
        cfg.set_version(CONFIG_VERSION);
        cfg.set_bump([bump]);
        cfg.set_authority(*self.accounts.authority.key());
        cfg.set_seat_authority(self.instruction_data.seat_authority);
        cfg.set_market_authority(self.instruction_data.market_authority);

        Ok(())
    }
}
