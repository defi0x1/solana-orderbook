use crate::errors::ClobError;
use crate::state::{BookRefMut, Market, SelfTradePolicy, Side, TakeParams};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

/// # PlaceLimit
///
/// Add or amend a single quote, leaving the maker's other quotes untouched —
/// the incremental counterpart to `MassQuote`, which replaces the whole set.
/// Unlike `MassQuote` it may cross: without the post-only flag it matches
/// against the opposing side up to the limit tick and rests the remainder.
///
/// A maker holds at most one order per (side, tick), so quoting a tick they
/// already quote amends it in place rather than stacking a second order:
/// shrinking keeps its place in the queue, growing re-posts at the back.
///
/// > Verify the seat belongs to the signer
/// > Post-only: insert directly (slide or reject on cross per flag)
/// > Otherwise: take() up to the limit, settle the taker leg on the seat,
/// >   then rest the remainder at the limit tick (cannot cross by then)
///
/// Accounts:
///
/// 1. owner:       [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. seat_index: u16,
/// 2. side: u8,            // 0 = bid, 1 = ask
/// 3. tick: u32,           // Absolute limit tick
/// 4. lots: u32,
/// 5. expiry_slot: u32,    // For the resting portion; 0 = GTC
/// 6. flags: u8,           // bit0 = post_only; bit1 = reject-on-cross when
///    post-only, abort-on-self-trade when crossing (mutually exclusive paths)
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner
/// - Market: owner, length and version via Market::check; must be writable
///
/// Instruction Checks:
/// - lots bounds and tick window are validated inside the Execution Helpers
///
/// Event Data: none — book state is read from account updates.
struct PlaceLimitAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for PlaceLimitAccounts<'a> {
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

struct PlaceLimitInstructionData {
    seat_index: u16,
    side: Side,
    tick: u32,
    lots: u32,
    expiry_slot: u32,
    flags: u8,
}

impl PlaceLimitInstructionData {
    #[inline(always)]
    fn post_only(&self) -> bool {
        self.flags & 0b0000_0001 != 0
    }

    #[inline(always)]
    fn reject_on_cross(&self) -> bool {
        self.flags & 0b0000_0010 != 0
    }
}

impl<'a> TryFrom<&'a [u8]> for PlaceLimitInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 16 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);
        let side = Side::try_from_u8(data[2])?;
        let tick = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
        let lots = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
        let expiry_slot = u32::from_le_bytes([data[11], data[12], data[13], data[14]]);
        let flags = data[15];

        Ok(Self {
            seat_index,
            side,
            tick,
            lots,
            expiry_slot,
            flags,
        })
    }
}

pub(crate) struct PlaceLimit<'a> {
    accounts: PlaceLimitAccounts<'a>,
    instruction_data: PlaceLimitInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for PlaceLimit<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("PlaceLimit");

        let accounts = PlaceLimitAccounts::try_from(accounts)?;
        let instruction_data = PlaceLimitInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> PlaceLimit<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 21;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;

        let seat = self.instruction_data.seat_index;
        let side = self.instruction_data.side;

        // A zero-size quote is an explicit cancel of this maker's quote at
        // the exact side and tick. It must never leave a live zero-lot node.
        if self.instruction_data.lots == 0 {
            book.upsert_order(
                side,
                self.instruction_data.tick,
                0,
                seat,
                self.instruction_data.expiry_slot,
                false,
            )?;
            book.header.bump_seq();
            return Ok(());
        }

        if self.instruction_data.post_only() {
            book.upsert_order(
                side,
                self.instruction_data.tick,
                self.instruction_data.lots,
                seat,
                self.instruction_data.expiry_slot,
                !self.instruction_data.reject_on_cross(),
            )?;
            book.header.bump_seq();
            return Ok(());
        }

        // Crossing path: match first, then rest the remainder.
        let slot = Clock::get()?.slot;
        let policy = SelfTradePolicy::from_flags(self.instruction_data.flags);
        let res = book.take(TakeParams {
            side,
            limit_tick: self.instruction_data.tick,
            max_lots: self.instruction_data.lots,
            max_quote: u64::MAX,
            seat,
            slot,
            self_trade: policy,
        })?;

        // Settle the taker leg on the seat's internal balances.
        if res.lots > 0 {
            let base_atoms = book.header.base_atoms(res.lots);
            let s = &mut book.seats[seat as usize];
            match side {
                Side::Bid => {
                    let cost = res.quote + res.fee;
                    if s.quote_free() < cost {
                        return Err(ClobError::InsufficientBalance.into());
                    }
                    s.set_quote_free(s.quote_free() - cost);
                    s.set_base_free(s.base_free().wrapping_add(base_atoms));
                }
                Side::Ask => {
                    if s.base_free() < base_atoms {
                        return Err(ClobError::InsufficientBalance.into());
                    }
                    s.set_base_free(s.base_free() - base_atoms);
                    s.set_quote_free(s.quote_free().wrapping_add(res.quote - res.fee));
                }
            }
        }

        // Rest the remainder: take() consumed everything within the limit, so
        // this can no longer cross.
        let remainder = self.instruction_data.lots - res.lots;
        if remainder > 0 {
            book.upsert_order(
                side,
                self.instruction_data.tick,
                remainder,
                seat,
                self.instruction_data.expiry_slot,
                false,
            )?;
        }

        book.header.bump_seq();
        Ok(())
    }
}
