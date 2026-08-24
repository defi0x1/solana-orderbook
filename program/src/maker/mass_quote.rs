use crate::errors::ClobError;
use crate::{
    constants::MAX_ORDERS_PER_SEAT,
    state::{BookRefMut, Market, Quote, Side},
};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # MassQuote
///
/// Replace the seat's entire slab quote set in one instruction. The maker hot
/// path: a market maker refreshing 40 two-sided levels does it here in a single
/// ~370-byte transaction instead of 40 cancel+posts.
///
/// > Verify the seat belongs to the signer
/// > Merge the bid payload against the seat chain's bid region (single pass)
/// > Merge the ask payload against the ask region
/// > Unchanged quotes only refresh their expiry (FIFO priority kept)
/// > Decreases shrink in place (priority kept); increases re-post at the tail
/// > Chain quotes missing from the payload are cancelled
/// > A quote whose post-only slide would stack on another of this maker's
/// >   quotes rejects the whole instruction (one order per (side, tick))
///
/// All quotes share the payload's expiry_slot: a maker quoting with
/// `expiry = now + 2` is never exposed for more than 2 slots even if it never
/// sends another transaction — the cancel race does not exist here.
///
/// Accounts:
///
/// 1. owner:       [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. seat_index: u16,
/// 2. expiry_slot: u32,    // Absolute slot; 0 = GTC
/// 3. bids: u8-prefixed array of [tick u32 | lots u32 | flags u8]
/// 4. asks: u8-prefixed array of [tick u32 | lots u32 | flags u8]
///    Each side sorted strictly ascending by tick.
///    Quote flags bit 0: 0 = slide on cross (default), 1 = reject on cross.
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner
/// - Market: owner, length and version via Market::check; must be writable
///
/// Instruction Checks:
/// - Payload length must be exactly 8 + 9 * (bid_count + ask_count)
/// - Unsorted payload -> PayloadNotSorted (detected during the merge, free)
///
/// Event Data: none — book state is read from account updates.
struct MassQuoteAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for MassQuoteAccounts<'a> {
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

struct MassQuoteInstructionData<'a> {
    seat_index: u16,
    expiry_slot: u32,
    bid_count: usize,
    ask_count: usize,
    bids: &'a [u8],
    asks: &'a [u8],
}

impl<'a> TryFrom<&'a [u8]> for MassQuoteInstructionData<'a> {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        // seat u16 | expiry u32 | bids: u8-prefixed | asks: u8-prefixed
        const HEADER: usize = 6;
        if data.len() < HEADER + 2 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);
        let expiry_slot = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);

        let bid_count = data[HEADER] as usize;
        let bids_start = HEADER + 1;
        let asks_len_at = bids_start + bid_count * Quote::LEN;
        if data.len() < asks_len_at + 1 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let ask_count = data[asks_len_at] as usize;
        let asks_start = asks_len_at + 1;
        if data.len().ne(&(asks_start + ask_count * Quote::LEN)) {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(Self {
            seat_index,
            expiry_slot,
            bid_count,
            ask_count,
            bids: &data[bids_start..asks_len_at],
            asks: &data[asks_start..],
        })
    }
}

pub(crate) struct MassQuote<'a> {
    accounts: MassQuoteAccounts<'a>,
    instruction_data: MassQuoteInstructionData<'a>,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for MassQuote<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("MassQuote");

        let accounts = MassQuoteAccounts::try_from(accounts)?;
        let instruction_data = MassQuoteInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> MassQuote<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 20;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;
        if self.instruction_data.bid_count + self.instruction_data.ask_count
            > MAX_ORDERS_PER_SEAT as usize
        {
            return Err(ClobError::TooManyOrders.into());
        }

        let head = book.seats[self.instruction_data.seat_index as usize].orders_head();
        let cursor = book.mass_quote_side(
            self.instruction_data.seat_index,
            Side::Bid,
            self.instruction_data.bids,
            self.instruction_data.bid_count,
            self.instruction_data.expiry_slot,
            (crate::constants::NIL, head),
        )?;
        book.mass_quote_side(
            self.instruction_data.seat_index,
            Side::Ask,
            self.instruction_data.asks,
            self.instruction_data.ask_count,
            self.instruction_data.expiry_slot,
            cursor,
        )?;

        book.header.bump_seq();
        Ok(())
    }
}
