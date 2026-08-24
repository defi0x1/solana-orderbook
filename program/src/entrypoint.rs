//! p-token-style entrypoint: the maker instructions are the preferred ones,
//! the way p-token prioritizes `transfer`. Makers quote continuously and
//! their cost sets the spread; takers cross it and can afford the generic
//! path.
//!
//! One hot shape. The maker set (MassQuote, Cancel, PlaceLimit) is exactly
//! two accounts, owner + market. The entrypoint probes the runtime's
//! serialized input buffer at computed offsets and, when the shape and a hot
//! discriminator match, materializes the `AccountInfo`s directly and jumps
//! into the handler — no generic account loop, no dispatch tree. Everything
//! else (takers, cranks, admin, seat, and every error path) lands in the
//! `#[cold]` fallback, which is byte-for-byte the standard pinocchio
//! entrypoint — cranks are rare by design and have not measured as worth a
//! fast path. Hot code stays linear; cold code stays out of it.
//!
//! The offsets below describe the SOLANA LOADER's input serialization, which
//! this program parses directly instead of walking it generically. The layout
//! is: a `u64` account count, then per non-duplicated account an 88-byte
//! header (`data_len` at +80), the account data, resize headroom, padding to
//! an 8-byte boundary, and a `u64` rent epoch; then a `u64` instruction data
//! length and the instruction data. It is the loader's ABI, not pinocchio's —
//! pinocchio only mirrors it — so it is stable across pinocchio versions and
//! changes only with the runtime itself.

use crate::state::likely;
use core::mem::{transmute, MaybeUninit};
use pinocchio::{
    account_info::{AccountInfo, MAX_PERMITTED_DATA_INCREASE},
    entrypoint::deserialize,
    ProgramResult, SUCCESS,
};

/// Marks an account record the runtime serialized in full; a duplicate carries
/// the index of the account it repeats instead.
const NON_DUP_MARKER: u8 = u8::MAX;
/// Per-account header the loader writes ahead of the account data:
/// borrow state, three flag bytes, original data length, key, owner, lamports,
/// and the data length at `DATA_LEN_OFFSET`.
const ACCOUNT_PREAMBLE: usize = 88;
const DATA_LEN_OFFSET: usize = 80;
/// Spare capacity the loader reserves after each account's data so a program
/// can grow it in place.
const RESIZE_PADDING: usize = MAX_PERMITTED_DATA_INCREASE;
/// Trailing rent epoch field, one per account.
const RENT_EPOCH: usize = 8;

#[inline(always)]
fn align8(v: usize) -> usize {
    (v + 7) & !7
}

#[no_mangle]
pub unsafe extern "C" fn entrypoint(input: *mut u8) -> u64 {
    let account_count = *(input as *const u64);
    let a1 = 8usize;

    // Maker shape: exactly two accounts (owner, market), both non-dup.
    if likely(account_count == 2) && likely(*input.add(a1) == NON_DUP_MARKER) {
        let dl1 = *(input.add(a1 + DATA_LEN_OFFSET) as *const u64) as usize;
        let a2 = align8(a1 + ACCOUNT_PREAMBLE + dl1 + RESIZE_PADDING) + RENT_EPOCH;

        if likely(*input.add(a2) == NON_DUP_MARKER) {
            let dl2 = *(input.add(a2 + DATA_LEN_OFFSET) as *const u64) as usize;
            let ix_len_at = align8(a2 + ACCOUNT_PREAMBLE + dl2 + RESIZE_PADDING) + RENT_EPOCH;
            let ix_len = *(input.add(ix_len_at) as *const u64) as usize;

            if likely(ix_len >= 1) {
                let discriminator = *input.add(ix_len_at + 8);
                let data = core::slice::from_raw_parts(input.add(ix_len_at + 8 + 1), ix_len - 1);

                /// Materialize the two accounts and run one hot handler. The
                /// borrow_state reset mirrors what `deserialize` does when it
                /// repurposes the dup marker as the borrow tracker.
                macro_rules! run_hot {
                    ($ix:ty) => {{
                        *input.add(a1) = 0;
                        *input.add(a2) = 0;
                        let accounts: [AccountInfo; 2] = [
                            transmute::<*mut u8, AccountInfo>(input.add(a1)),
                            transmute::<*mut u8, AccountInfo>(input.add(a2)),
                        ];
                        return match <$ix>::try_from((data, &accounts[..]))
                            .and_then(|ix| ix.process())
                        {
                            Ok(()) => SUCCESS,
                            Err(error) => error.into(),
                        };
                    }};
                }

                // The maker set, most frequent first: the bulk refresh, then
                // the single cancel, then the single quote. Anything else —
                // including the takers — falls through to the cold path.
                if discriminator == crate::MassQuote::DISCRIMINATOR {
                    run_hot!(crate::MassQuote);
                } else if discriminator == crate::Cancel::DISCRIMINATOR {
                    run_hot!(crate::Cancel);
                } else if discriminator == crate::PlaceLimit::DISCRIMINATOR {
                    run_hot!(crate::PlaceLimit);
                }
            }
        }
    }

    fallback(input)
}

/// The standard pinocchio entrypoint (what `program_entrypoint!` expands to):
/// admin, seat, swap and hygiene instructions, plus any call whose shape did
/// not match the fast path. Cold on purpose — it must never cost the hot path
/// a single instruction of code layout.
#[cold]
unsafe fn fallback(input: *mut u8) -> u64 {
    const UNINIT: MaybeUninit<AccountInfo> = MaybeUninit::<AccountInfo>::uninit();
    let mut accounts = [UNINIT; { pinocchio::MAX_TX_ACCOUNTS }];

    let (program_id, count, instruction_data) =
        deserialize::<{ pinocchio::MAX_TX_ACCOUNTS }>(input, &mut accounts);

    match crate::process_instruction(
        program_id,
        core::slice::from_raw_parts(accounts.as_ptr() as _, count),
        instruction_data,
    ) {
        Ok(()) => SUCCESS,
        Err(error) => error.into(),
    }
}

/// The handlers return `ProgramResult`; keep the signature nailed down so a
/// drift in the generic dispatcher shows up here at compile time.
const _: fn(&pinocchio::pubkey::Pubkey, &[AccountInfo], &[u8]) -> ProgramResult =
    crate::process_instruction;
