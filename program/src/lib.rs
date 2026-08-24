#![cfg_attr(not(target_os = "solana"), allow(dead_code, unused_imports))]

use pinocchio::{
    account_info::AccountInfo, default_panic_handler, no_allocator, program_error::ProgramError,
    pubkey::Pubkey, ProgramResult,
};

// The custom entrypoint handles the three maker instructions directly and
// sends every other shape through `process_instruction`. The program never
// allocates; `no_allocator!` turns an accidental heap use into a hard failure.
#[cfg(target_os = "solana")]
mod entrypoint;
no_allocator!();
default_panic_handler!();

mod admin;
use admin::*;

mod seat;
use seat::*;

mod maker;
use maker::*;

mod taker;
use taker::*;

mod hygiene;
use hygiene::*;

mod constants;
mod errors;
mod helpers;
mod state;

// BzcBf4JYcpWhFuw1smYiBSt647PJraT1Wxm2soBTSsvb
pub const ID: Pubkey = [
    0xa3, 0x56, 0xc1, 0xfb, 0xbd, 0xc3, 0xb9, 0x74, 0x17, 0xb8, 0x8c, 0x6d, 0xc1, 0xa1, 0xb7, 0xe5,
    0x44, 0xa8, 0x91, 0xed, 0x95, 0x17, 0x6e, 0x9e, 0x9e, 0x21, 0x37, 0xaa, 0x31, 0x1a, 0x40, 0x74,
];

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (discriminator, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match *discriminator {
        CreateMarket::DISCRIMINATOR => CreateMarket::try_from((data, accounts))?.process(),
        UpdateFee::DISCRIMINATOR => UpdateFee::try_from((data, accounts))?.process(),
        CollectFees::DISCRIMINATOR => CollectFees::try_from(accounts)?.process(),
        CreateConfig::DISCRIMINATOR => CreateConfig::try_from((data, accounts))?.process(),
        UpdateConfig::DISCRIMINATOR => UpdateConfig::try_from((data, accounts))?.process(),
        ClaimSeat::DISCRIMINATOR => ClaimSeat::try_from(accounts)?.process(),
        CloseSeat::DISCRIMINATOR => CloseSeat::try_from((data, accounts))?.process(),
        Deposit::DISCRIMINATOR => Deposit::try_from((data, accounts))?.process(),
        Withdraw::DISCRIMINATOR => Withdraw::try_from((data, accounts))?.process(),
        MassQuote::DISCRIMINATOR => MassQuote::try_from((data, accounts))?.process(),
        PlaceLimit::DISCRIMINATOR => PlaceLimit::try_from((data, accounts))?.process(),
        Cancel::DISCRIMINATOR => Cancel::try_from((data, accounts))?.process(),
        PlaceTake::DISCRIMINATOR => PlaceTake::try_from((data, accounts))?.process(),
        Swap::DISCRIMINATOR => Swap::try_from((data, accounts))?.process(),
        Reanchor::DISCRIMINATOR => Reanchor::try_from((data, accounts))?.process(),
        ReapExpired::DISCRIMINATOR => ReapExpired::try_from((data, accounts))?.process(),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

#[cfg(feature = "test-utils")]
pub mod test_utils {
    pub use crate::constants::*;
    pub use crate::state::{BookRefMut, Market, SelfTradePolicy, Side, TakeResult};

    /// True iff `e` is the program's "book is corrupt, abort" sentinel
    /// (`ClobError::InvalidState`). `ClobError` itself is `pub(crate)` — its
    /// variants are an implementation detail, not part of the account/wire
    /// ABI — so out-of-crate harnesses (the `fuzz/` targets) that want to
    /// treat this one specific outcome as a hard invariant violation, and
    /// every other `Err` as ordinary rejected input, go through this instead
    /// of matching a raw discriminant.
    pub fn is_invalid_state(e: &pinocchio::program_error::ProgramError) -> bool {
        matches!(
            e,
            pinocchio::program_error::ProgramError::Custom(c)
                if *c == crate::errors::ClobError::InvalidState as u32
        )
    }
}
