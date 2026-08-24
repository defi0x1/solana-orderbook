use crate::constants::NIL;
use crate::errors::ClobError;
use crate::state::{BookRefMut, Market};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # CloseSeat
///
/// Release a seat slot. The seat must be fully empty: zero balances and no
/// resting orders (`order_count` is exact — cancel first, e.g. via a
/// `MassQuote` with an empty payload).
///
/// Accounts:
///
/// 1. owner:       [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. seat_index: u16,
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner
/// - Market: owner, length and version via Market::check; must be writable
///
/// Instruction Checks:
/// - Non-zero balances or live orders -> SeatNotEmpty
///
/// Event Data: none.
struct CloseSeatAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CloseSeatAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [owner, market] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !owner.is_signer() {
            return Err(ClobError::NotSigner.into());
        }
        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Market::check(market)?;

        Ok(Self { owner, market })
    }
}

struct CloseSeatInstructionData {
    seat_index: u16,
}

impl<'a> TryFrom<&'a [u8]> for CloseSeatInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 2 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);

        Ok(Self { seat_index })
    }
}

pub(crate) struct CloseSeat<'a> {
    accounts: CloseSeatAccounts<'a>,
    instruction_data: CloseSeatInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CloseSeat<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CloseSeat");

        let accounts = CloseSeatAccounts::try_from(accounts)?;
        let instruction_data = CloseSeatInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> CloseSeat<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 11;

    pub(crate) fn process(self) -> ProgramResult {
        let book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;
        let seat_index = self.instruction_data.seat_index;

        let seat = &mut book.seats[seat_index as usize];
        if seat.base_free() != 0
            || seat.quote_free() != 0
            || seat.base_locked() != 0
            || seat.quote_locked() != 0
            || seat.order_count() != 0
        {
            return Err(ClobError::SeatNotEmpty.into());
        }

        seat.set_owner([0u8; 32]);
        seat.set_orders_head(NIL);
        book.header.bump_seq();

        Ok(())
    }
}
