use crate::errors::ClobError;
use crate::helpers::log_reanchor_event;
use crate::state::{BookRefMut, Config, Market};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Reanchor
///
/// Move the market's price window so it starts at `target_anchor`, evicting
/// every order that falls outside the new range. Evictions refund through seat
/// balances, so this can never fail on a refund, and the ring layout means no
/// memory moves — only the anchor label changes.
///
/// The window is finite, so a market whose price leaves it needs the range
/// relocated. That is an operational decision about where this market trades,
/// which is why it is gated on the config's market authority and why the
/// target is stated outright rather than inferred from the current book: a
/// price derived from resting orders is a price someone else can choose for
/// you.
///
/// Bounded by `max_levels` occupied levels per call; call repeatedly to
/// complete a long move. The book is consistent after every call.
///
/// Accounts:
///
/// 1. market_authority:    [signer]
/// 2. config:
/// 3. market:              [mut]
///
/// Parameters:
/// 1. target_anchor: u32,  // Rounded down to a bitmap word, clamped in range
/// 2. max_levels: u32,
///
/// Account Checks:
/// - Market authority: must sign and match the config's market_authority
/// - Config: owner, length and version via Config::check, and must be the
///   config this market named at creation
/// - Market: owner, length and version via Market::check; must be writable
///
/// Event Data:
/// - discriminator: u8, (4u8)
/// - market: Pubkey,
/// - new_anchor: u32,
/// - evicted: u32,
struct ReanchorAccounts<'a> {
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for ReanchorAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [market_authority, config, market] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !market_authority.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Market::check(market)?;
        Config::check(config)?;

        let header = unsafe { Market::from_bytes_unchecked(market.borrow_data_unchecked()) };
        if header.config().ne(config.key()) {
            return Err(ClobError::InvalidProgramAuthority.into());
        }
        let cfg = unsafe { Config::from_bytes_unchecked(config.borrow_data_unchecked()) };
        if cfg.market_authority().ne(market_authority.key()) {
            return Err(ClobError::InvalidProgramAuthority.into());
        }

        Ok(Self { market })
    }
}

struct ReanchorInstructionData {
    target_anchor: u32,
    max_levels: u32,
}

impl<'a> TryFrom<&'a [u8]> for ReanchorInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 8 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let target_anchor = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let max_levels = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        Ok(Self {
            target_anchor,
            max_levels,
        })
    }
}

pub(crate) struct Reanchor<'a> {
    accounts: ReanchorAccounts<'a>,
    instruction_data: ReanchorInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Reanchor<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Reanchor");

        let accounts = ReanchorAccounts::try_from(accounts)?;
        let instruction_data = ReanchorInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Reanchor<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 40;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        let (evicted, new_anchor) = book.reanchor_step(
            self.instruction_data.target_anchor,
            self.instruction_data.max_levels,
        )?;
        book.header.bump_seq();

        log_reanchor_event(self.accounts.market.key(), new_anchor, evicted);

        Ok(())
    }
}
