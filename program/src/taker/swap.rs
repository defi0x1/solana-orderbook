use crate::errors::ClobError;
use crate::helpers::{token_amount, CheckedTransfer};
use crate::state::{BookRefMut, Market, SelfTradePolicy, Side, TakeParams, TakeResult};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

/// # Swap
///
/// Seatless exact-in taker: deposit-free flow for aggregators (the Jupiter
/// path). Two-to-three token CPIs dominate the cost — anyone latency- or
/// fee-sensitive should use a seat and PlaceTake instead.
///
/// > Pull the input leg from the user's token account into the vault FIRST,
/// >   and read back the vault's observed balance delta
/// > Match against the book using that OBSERVED delta as the budget — not
/// >   the instruction-stated `amount_in`
/// > Refund whatever the match didn't consume back to the user
/// > Push the output leg from the vault, signed by the vault authority PDA
///
/// Transfer-then-match (not match-then-transfer) is the load-bearing design
/// choice here, and it is NOT optional under a Token-2022 transfer fee: if
/// the match instead ran first against the nominal `amount_in` and the input
/// leg were pulled afterward, a fee-bearing input mint would ALWAYS deliver
/// less than the match assumed, on every single swap — not an edge case, the
/// common case, since a fee-bearing mint charges its fee on every transfer.
/// An earlier version of this instruction reverted on that shortfall instead
/// of avoiding it, which made Swap permanently unusable for exactly the
/// mints Token-2022 support exists to serve. Matching against the observed
/// delta instead means there is never a shortfall to detect: whatever
/// arrived is what gets matched.
///
/// Pulling the input before knowing how much the book can fill means the
/// match may not consume all of it — a thin book, or (selling base) a delta
/// that doesn't divide evenly into whole lots. That unmatched remainder is
/// real money already sitting in the vault, credited to nobody; it is
/// refunded back to the user's own input-side ATA before the instruction
/// finishes (a third CPI, only when the remainder is nonzero — a deep book
/// consuming the whole delta is the common case and pays for two CPIs, same
/// as before). Vault solvency is exact either way — the refund is a plain
/// bookkeeping transfer, not a workaround for anything unsafe.
///
/// Against a fee-bearing input mint specifically, note this refund is itself
/// a transfer of that same mint and pays that same fee again: a caller who
/// sizes `amount_in` well above what a thin book can fill, or (selling base)
/// leaves a remainder under one lot, loses more to fees than a naive
/// pre-fee estimate would suggest. Integrators should size `amount_in` to
/// what the book can plausibly fill rather than relying on the refund as a
/// free undo.
///
/// A failed CPI is not something this program can catch or partially back
/// out of: `invoke_signed` propagates a callee failure as an unrecoverable
/// syscall failure, there is no `Result` to inspect and continue past. And
/// the guarantee that matters here is at the TRANSACTION level, not this
/// instruction's: if this instruction returns an error for any reason — the
/// output-leg transfer failing, say — the runtime discards every account
/// write the whole transaction made, not just ones this instruction made
/// syntactically "after" some point. There is no partial-application window
/// for a caller to observe or exploit; either every write here lands, or
/// none of them do, including the input-leg pull and the match's writes to
/// the book.
///
/// Accounts:
///
/// 1. user:                  [signer]
/// 2. market:                 [mut]
/// 3. user_base_ata:          [mut]
/// 4. user_quote_ata:         [mut]
/// 5. base_vault:              [mut]
/// 6. quote_vault:             [mut]
/// 7. vault_authority:
/// 8. base_mint:
/// 9. quote_mint:
/// 10. base_token_program:    [executable]
/// 11. quote_token_program:   [executable]
///
/// Parameters:
/// 1. side: u8,        // 0 = buy base (amount_in = quote atoms), 1 = sell base
///    // (amount_in = base atoms)
/// 2. amount_in: u64,
/// 3. limit_tick: u32, // Worst acceptable price
/// 4. min_out: u64,    // Output atoms, post-fee
///
/// Account Checks:
/// - User: no need to check signer — the input-leg transfer fails without it
/// - Market: owner, length and version via Market::check; must be writable
/// - Vaults + vault_authority: compared against the addresses stored in the
///   header (~30 CU, no PDA derivation on this path)
/// - User ATAs: no need to check — the transfers fail on wrong mint or owner
/// - Mints: no need to check beyond what TransferChecked itself enforces
/// - Token programs: each compared against the header's recorded flag for
///   that asset
///
/// Instruction Checks:
/// - amount_in: no need to check — the input transfer fails if underfunded
///
/// Event Data: none — book state is read from account updates.
struct SwapAccounts<'a> {
    user: &'a AccountInfo,
    market: &'a AccountInfo,
    user_base_ata: &'a AccountInfo,
    user_quote_ata: &'a AccountInfo,
    base_vault: &'a AccountInfo,
    quote_vault: &'a AccountInfo,
    vault_authority: &'a AccountInfo,
    base_mint: &'a AccountInfo,
    quote_mint: &'a AccountInfo,
    base_token_program: &'a AccountInfo,
    quote_token_program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for SwapAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [user, market, user_base_ata, user_quote_ata, base_vault, quote_vault, vault_authority, base_mint, quote_mint, base_token_program, quote_token_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !market.is_writable() {
            return Err(ClobError::NotMutable.into());
        }
        Market::check(market)?;

        let header = unsafe { Market::from_bytes_unchecked(market.borrow_data_unchecked()) };
        if base_vault.key().ne(header.base_vault()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if quote_vault.key().ne(header.quote_vault()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if vault_authority.key().ne(header.vault_authority()) {
            return Err(ClobError::InvalidTokenAddress.into());
        }
        if base_token_program.key().ne(&header.base_token_program_id()) {
            return Err(ClobError::UnsupportedTokenProgram.into());
        }
        if quote_token_program
            .key()
            .ne(&header.quote_token_program_id())
        {
            return Err(ClobError::UnsupportedTokenProgram.into());
        }

        Ok(Self {
            user,
            market,
            user_base_ata,
            user_quote_ata,
            base_vault,
            quote_vault,
            vault_authority,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
        })
    }
}

struct SwapInstructionData {
    side: Side,
    amount_in: u64,
    limit_tick: u32,
    min_out: u64,
}

impl<'a> TryFrom<&'a [u8]> for SwapInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 21 {
            return Err(ProgramError::InvalidInstructionData);
        }

        let side = Side::try_from_u8(data[0])?;
        let amount_in = u64::from_le_bytes([
            data[1], data[2], data[3], data[4], data[5], data[6], data[7], data[8],
        ]);
        let limit_tick = u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
        let min_out = u64::from_le_bytes([
            data[13], data[14], data[15], data[16], data[17], data[18], data[19], data[20],
        ]);

        Ok(Self {
            side,
            amount_in,
            limit_tick,
            min_out,
        })
    }
}

pub(crate) struct Swap<'a> {
    accounts: SwapAccounts<'a>,
    instruction_data: SwapInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Swap<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("Swap");

        let accounts = SwapAccounts::try_from(accounts)?;
        let instruction_data = SwapInstructionData::try_from(data)?;

        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Swap<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 31;

    pub(crate) fn process(self) -> ProgramResult {
        let slot = Clock::get()?.slot;

        // This `&mut` stays alive across every CPI below (`borrow_mut_data_
        // unchecked` doesn't set pinocchio's runtime borrow flag, so nothing
        // here relies on that mechanism to prevent aliasing). Safety instead
        // rests on `self.accounts.market` never appearing as a CPI account:
        // `Market::check` in `SwapAccounts::try_from` already requires it be
        // owned by this program, so passing it to either token program as a
        // vault/mint/ATA argument fails that program's own owner check
        // before any write-back — including read-only metas, which never
        // write back regardless. No key-inequality guard is added here on
        // purpose: it would duplicate a check the callee already makes.
        let mut book = unsafe {
            BookRefMut::from_bytes_unchecked_mut(self.accounts.market.borrow_mut_data_unchecked())
        };
        let base_decimals = book.header.base_decimals();
        let quote_decimals = book.header.quote_decimals();
        let bump = *book.header.bump();
        let market_key = self.accounts.market.key();
        let seeds = [
            Seed::from(crate::constants::VAULT_SEED),
            Seed::from(market_key.as_ref()),
            Seed::from(bump.as_ref()),
        ];

        match self.instruction_data.side {
            Side::Bid => {
                // Pull the quote leg first; match against what actually
                // arrived, not the nominal amount_in.
                let pre = unsafe { token_amount(self.accounts.quote_vault)? };
                CheckedTransfer {
                    token_program: self.accounts.quote_token_program,
                    from: self.accounts.user_quote_ata,
                    mint: self.accounts.quote_mint,
                    to: self.accounts.quote_vault,
                    authority: self.accounts.user,
                    amount: self.instruction_data.amount_in,
                    decimals: quote_decimals,
                }
                .invoke()?;
                let post = unsafe { token_amount(self.accounts.quote_vault)? };
                let delta = post.checked_sub(pre).ok_or(ClobError::InvalidState)?;

                let res = book.take(TakeParams {
                    side: Side::Bid,
                    limit_tick: self.instruction_data.limit_tick,
                    max_lots: book.header.max_lots_per_order(),
                    max_quote: delta,
                    seat: crate::constants::NO_SEAT,
                    slot,
                    self_trade: SelfTradePolicy::CancelResting,
                })?;
                book.header.bump_seq();

                let base_atoms = book.header.base_atoms(res.lots);
                if base_atoms < self.instruction_data.min_out {
                    return Err(ClobError::SlippageExceeded.into());
                }

                // take()'s own budget cap guarantees consumed <= delta, so
                // this never underflows.
                let consumed = res.quote + res.fee;
                let leftover = delta - consumed;
                if leftover > 0 {
                    CheckedTransfer {
                        token_program: self.accounts.quote_token_program,
                        from: self.accounts.quote_vault,
                        mint: self.accounts.quote_mint,
                        to: self.accounts.user_quote_ata,
                        authority: self.accounts.vault_authority,
                        amount: leftover,
                        decimals: quote_decimals,
                    }
                    .invoke_signed(&[Signer::from(&seeds)])?;
                }

                // Push the base leg.
                CheckedTransfer {
                    token_program: self.accounts.base_token_program,
                    from: self.accounts.base_vault,
                    mint: self.accounts.base_mint,
                    to: self.accounts.user_base_ata,
                    authority: self.accounts.vault_authority,
                    amount: base_atoms,
                    decimals: base_decimals,
                }
                .invoke_signed(&[Signer::from(&seeds)])?;
            }
            Side::Ask => {
                // Pull the base leg first; same reorder as the Bid arm.
                let pre = unsafe { token_amount(self.accounts.base_vault)? };
                CheckedTransfer {
                    token_program: self.accounts.base_token_program,
                    from: self.accounts.user_base_ata,
                    mint: self.accounts.base_mint,
                    to: self.accounts.base_vault,
                    authority: self.accounts.user,
                    amount: self.instruction_data.amount_in,
                    decimals: base_decimals,
                }
                .invoke()?;
                let post = unsafe { token_amount(self.accounts.base_vault)? };
                let delta = post.checked_sub(pre).ok_or(ClobError::InvalidState)?;

                // Lot-budgeted matching against the observed delta. A delta
                // under one lot (dust, or a book so thin nothing can fill)
                // means there is nothing valid to hand take() — max_lots == 0
                // is itself a hard error inside take() — so skip the call
                // entirely and let the refund below return the whole delta.
                let lots_in = delta / book.header.base_lot_size();
                let max_lots = core::cmp::min(lots_in, book.header.max_lots_per_order() as u64);
                let res = if max_lots == 0 {
                    TakeResult::default()
                } else {
                    book.take(TakeParams {
                        side: Side::Ask,
                        limit_tick: self.instruction_data.limit_tick,
                        max_lots: max_lots as u32,
                        max_quote: u64::MAX,
                        seat: crate::constants::NO_SEAT,
                        slot,
                        self_trade: SelfTradePolicy::CancelResting,
                    })?
                };
                book.header.bump_seq();

                let out = res.quote - res.fee;
                if out < self.instruction_data.min_out {
                    return Err(ClobError::SlippageExceeded.into());
                }

                // lots_in = floor(delta / base_lot_size) and res.lots <=
                // lots_in, so consumed_base <= delta always.
                let consumed_base = book.header.base_atoms(res.lots);
                let leftover = delta - consumed_base;
                if leftover > 0 {
                    CheckedTransfer {
                        token_program: self.accounts.base_token_program,
                        from: self.accounts.base_vault,
                        mint: self.accounts.base_mint,
                        to: self.accounts.user_base_ata,
                        authority: self.accounts.vault_authority,
                        amount: leftover,
                        decimals: base_decimals,
                    }
                    .invoke_signed(&[Signer::from(&seeds)])?;
                }

                // Push the quote leg (post-fee).
                CheckedTransfer {
                    token_program: self.accounts.quote_token_program,
                    from: self.accounts.quote_vault,
                    mint: self.accounts.quote_mint,
                    to: self.accounts.user_quote_ata,
                    authority: self.accounts.vault_authority,
                    amount: out,
                    decimals: quote_decimals,
                }
                .invoke_signed(&[Signer::from(&seeds)])?;
            }
        }

        Ok(())
    }
}
