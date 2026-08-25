use crate::errors::ClobError;
use crate::helpers::log_claim_seat_event;
use crate::state::{BookRefMut, Config, Market};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # ClaimSeat
///
/// Grant `owner` the first free slot in the seat table. A seat carries the
/// holder's balances and their orders, and the table is finite, so admission
/// is a decision the venue makes rather than a race.
///
/// Only the seat authority signs. The grantee is named, not present — a venue
/// provisions seats for the makers it has onboarded without needing them to
/// co-sign, and receiving a seat costs the grantee nothing until they fund it.
///
/// The scan for a free slot is linear, which is the right trade — it keeps the
/// hot path free of any directory structure, and this instruction runs once
/// per participant.
///
/// Accounts:
///
/// 1. seat_authority:  [signer]
/// 2. owner:
/// 3. config:
/// 4. market:          [mut]
///
/// Parameters: none
///
/// Account Checks:
/// - Seat authority: must sign and match the config's seat_authority
/// - Owner: never read, only recorded. That key is then the authorization for
///   every later maker instruction on this seat. Rejected if it's the zero
///   address, which `Seat::is_free()` reserves to mean "slot unclaimed"
/// - Config: owner, length and version via Config::check, and must be the
///   config this market named at creation
/// - Market: owner, length and version via Market::check; must be writable
///
/// Event Data:
/// - discriminator: u8, (5u8)
/// - market: Pubkey,
/// - owner: Pubkey,
/// - seat_index: u16,
struct ClaimSeatAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for ClaimSeatAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [seat_authority, owner, config, market] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !seat_authority.is_signer() {
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
        if cfg.seat_authority().ne(seat_authority.key()) {
            return Err(ClobError::InvalidProgramAuthority.into());
        }
        // Seat::is_free() treats owner == [0u8; 32] as "slot unclaimed".
        if owner.key().eq(&[0u8; 32]) {
            return Err(ClobError::ZeroAuthority.into());
        }

        Ok(Self { owner, market })
    }
}

pub(crate) struct ClaimSeat<'a> {
    accounts: ClaimSeatAccounts<'a>,
}

impl<'a> TryFrom<&'a [AccountInfo]> for ClaimSeat<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("ClaimSeat");

        let accounts = ClaimSeatAccounts::try_from(accounts)?;

        Ok(Self { accounts })
    }
}

impl<'a> ClaimSeat<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 10;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        let seat_index = book.claim_seat(self.accounts.owner.key())?;
        book.header.bump_seq();

        log_claim_seat_event(
            self.accounts.market.key(),
            self.accounts.owner.key(),
            seat_index,
        );

        Ok(())
    }
}
