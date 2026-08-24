use crate::errors::ClobError;
use crate::state::{BookRefMut, Market, SelfTradePolicy, Side, TakeParams};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

/// # PlaceTake
///
/// Immediate-or-cancel taker order settled on the seat's internal balances.
/// The zero-CPI taker path: two accounts, no token movement, ~60 CU per fill.
///
/// > Verify the seat belongs to the signer
/// > Match against the opposing side up to limit_tick / max_lots
/// > Expired makers encountered mid-walk are reaped for free
/// > Settle the taker leg on the seat; enforce min_out (post-fee)
///
/// Accounts:
///
/// 1. owner:       [signer]
/// 2. market:      [mut]
///
/// Parameters:
/// 1. seat_index: u16,
/// 2. side: u8,            // 0 = bid (buy base), 1 = ask (sell base)
/// 3. limit_tick: u32,     // Worst acceptable price
/// 4. max_lots: u32,
/// 5. min_out: u64,        // Buy: base atoms out; sell: quote atoms out, post-fee
/// 6. flags: u8,           // bit1 = abort on self trade (default: cancel resting)
///
/// Account Checks:
/// - Owner: must be a signer; compared against seat.owner
/// - Market: owner, length and version via Market::check; must be writable
///
/// Instruction Checks:
/// - Balance sufficiency is checked at settlement: on failure the whole
///   instruction reverts, so partial state can never persist
///
/// Event Data: none — book state is read from account updates.
struct PlaceTakeAccounts<'a> {
    owner: &'a AccountInfo,
    market: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for PlaceTakeAccounts<'a> {
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

struct PlaceTakeInstructionData {
    seat_index: u16,
    side: Side,
    limit_tick: u32,
    max_lots: u32,
    min_out: u64,
    flags: u8,
}

impl<'a> TryFrom<&'a [u8]> for PlaceTakeInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 20 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let seat_index = u16::from_le_bytes([data[0], data[1]]);
        let side = Side::try_from_u8(data[2])?;
        let limit_tick = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
        let max_lots = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
        let min_out = u64::from_le_bytes([
            data[11], data[12], data[13], data[14], data[15], data[16], data[17], data[18],
        ]);
        let flags = data[19];

        Ok(Self {
            seat_index,
            side,
            limit_tick,
            max_lots,
            min_out,
            flags,
        })
    }
}

pub(crate) struct PlaceTake<'a> {
    accounts: PlaceTakeAccounts<'a>,
    instruction_data: PlaceTakeInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for PlaceTake<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("PlaceTake");

        let accounts = PlaceTakeAccounts::try_from(accounts)?;
        let instruction_data = PlaceTakeInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> PlaceTake<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 30;

    pub(crate) fn process(self) -> ProgramResult {
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };

        // Authorization: the seat must belong to the signer
        book.check_seat(self.instruction_data.seat_index, self.accounts.owner.key())?;

        let seat = self.instruction_data.seat_index;
        let side = self.instruction_data.side;
        let slot = Clock::get()?.slot;
        let policy = SelfTradePolicy::from_flags(self.instruction_data.flags);

        let res = book.take(TakeParams {
            side,
            limit_tick: self.instruction_data.limit_tick,
            max_lots: self.instruction_data.max_lots,
            max_quote: u64::MAX,
            seat,
            slot,
            self_trade: policy,
        })?;

        // Settle the taker leg and enforce min_out (post-fee).
        let base_atoms = book.header.base_atoms(res.lots);
        let s = &mut book.seats[seat as usize];
        match side {
            Side::Bid => {
                if base_atoms < self.instruction_data.min_out {
                    return Err(ClobError::SlippageExceeded.into());
                }
                let cost = res.quote + res.fee;
                if s.quote_free() < cost {
                    return Err(ClobError::InsufficientBalance.into());
                }
                s.set_quote_free(s.quote_free() - cost);
                s.set_base_free(s.base_free().wrapping_add(base_atoms));
            }
            Side::Ask => {
                let out = res.quote - res.fee;
                if out < self.instruction_data.min_out {
                    return Err(ClobError::SlippageExceeded.into());
                }
                if s.base_free() < base_atoms {
                    return Err(ClobError::InsufficientBalance.into());
                }
                s.set_base_free(s.base_free() - base_atoms);
                s.set_quote_free(s.quote_free().wrapping_add(out));
            }
        }

        book.header.bump_seq();
        Ok(())
    }
}
