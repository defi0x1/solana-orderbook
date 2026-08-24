use crate::state::{BookRefMut, Market, Side};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

/// # ReapExpired
///
/// Permissionless hygiene: remove expired quotes on one side, refunding each
/// maker through their seat balance. Matching already reaps expired quotes it
/// walks over for free; this crank exists for stale levels no taker crosses —
/// anyone may run it, because it can only do what expiry already promised.
///
/// Bounded by `max_count` orders per call; call again to continue. The walk
/// starts at `start_tick` (clamped into the window), so a caller can resume
/// where the budget ran out.
///
/// Accounts:
///
/// 1. market:      [mut]
///
/// Parameters:
/// 1. side: u8,            // 0 = bids, 1 = asks
/// 2. start_tick: u32,     // Clamped into the current window
/// 3. max_count: u32,      // Max orders reaped this call
///
/// Account Checks:
/// - Market: owner, length and version via Market::check; must be writable
///
/// Event Data: none — book state is read from account updates.
struct ReapExpiredAccounts<'a> {
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for ReapExpiredAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [market] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !market.is_writable() {
            return Err(crate::errors::ClobError::NotMutable.into());
        }
        Market::check(market)?;

        Ok(Self { market })
    }
}

struct ReapExpiredInstructionData {
    side: Side,
    start_tick: u32,
    max_count: u32,
}

impl<'a> TryFrom<&'a [u8]> for ReapExpiredInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 9 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let side = Side::try_from_u8(data[0])?;
        let start_tick = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let max_count = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);

        Ok(Self {
            side,
            start_tick,
            max_count,
        })
    }
}

pub(crate) struct ReapExpired<'a> {
    accounts: ReapExpiredAccounts<'a>,
    instruction_data: ReapExpiredInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for ReapExpired<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("ReapExpired");

        let accounts = ReapExpiredAccounts::try_from(accounts)?;
        let instruction_data = ReapExpiredInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> ReapExpired<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 41;

    pub(crate) fn process(self) -> ProgramResult {
        let slot = Clock::get()?.slot;

        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        book.reap_expired(
            self.instruction_data.side,
            self.instruction_data.start_tick,
            self.instruction_data.max_count,
            slot,
        )?;
        book.header.bump_seq();

        Ok(())
    }
}
