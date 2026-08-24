use crate::errors::ClobError;
use crate::state::{BookRefMut, Market};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Cancel
///
/// Remove a single resting order and release its locked funds back to the
/// maker's seat balance.
///
/// > Verify the seat belongs to the signer
/// > Verify the node's order id matches (ABA protection against slot reuse)
/// > Unlink the node from its level FIFO and seat chain — both O(1), the
/// >   node carries its own back-links — refund the lock, free the node
///
/// Accounts:
///
/// 1. owner:       [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. seat_index: u16,     // Index into the seat table
/// 2. node_index: u16,     // Index into the order pool
/// 3. order_seq: u64,      // Expected node seq (the order id)
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner (nothing downstream
///   would fail otherwise — this check IS the authorization)
/// - Market: owner, length and version via Market::check; must be writable
///
/// Instruction Checks:
/// - node_index out of range or seq mismatch -> OrderNotFound (slot reused)
///
/// Event Data: none — book state is read from account updates.
struct CancelAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CancelAccounts<'a> {
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

struct CancelInstructionData {
    seat_index: u16,
    node_index: u16,
    order_seq: u64,
}

impl<'a> TryFrom<&'a [u8]> for CancelInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 12 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);
        let node_index = u16::from_le_bytes([data[2], data[3]]);
        let order_seq = u64::from_le_bytes([
            data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11],
        ]);

        Ok(Self {
            seat_index,
            node_index,
            order_seq,
        })
    }
}

pub(crate) struct Cancel<'a> {
    accounts: CancelAccounts<'a>,
    instruction_data: CancelInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Cancel<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("Cancel");

        let accounts = CancelAccounts::try_from(accounts)?;
        let instruction_data = CancelInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Cancel<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 22;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;

        // ABA-protected removal: unlink, refund the lock, free the node
        book.cancel_order(
            self.instruction_data.seat_index,
            self.instruction_data.node_index,
            self.instruction_data.order_seq,
        )?;

        book.header.bump_seq();
        Ok(())
    }
}
