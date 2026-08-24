//! Rust client for the clob program.

#[allow(dead_code, unused_imports, unexpected_cfgs)]
mod generated;

pub use generated::programs::CLOB_ID;

pub mod accounts {
    pub use crate::generated::accounts::*;
}

pub mod errors {
    pub use crate::generated::errors::ClobError;
}

pub mod instructions {
    pub use crate::generated::instructions::{
        Cancel, CancelInstructionArgs, ClaimSeat, CloseSeat, CloseSeatInstructionArgs, CollectFees,
        CreateConfig, CreateConfigInstructionArgs, CreateMarket, CreateMarketInstructionArgs,
        Deposit, DepositInstructionArgs, PlaceLimit, PlaceLimitInstructionArgs, PlaceTake,
        PlaceTakeInstructionArgs, Reanchor, ReanchorInstructionArgs, ReapExpired,
        ReapExpiredInstructionArgs, Swap, SwapInstructionArgs, UpdateConfig,
        UpdateConfigInstructionArgs, UpdateFee, UpdateFeeInstructionArgs, Withdraw,
        WithdrawInstructionArgs, CANCEL_DISCRIMINATOR, CLAIM_SEAT_DISCRIMINATOR,
        CLOSE_SEAT_DISCRIMINATOR, COLLECT_FEES_DISCRIMINATOR, CREATE_CONFIG_DISCRIMINATOR,
        CREATE_MARKET_DISCRIMINATOR, DEPOSIT_DISCRIMINATOR, PLACE_LIMIT_DISCRIMINATOR,
        PLACE_TAKE_DISCRIMINATOR, REANCHOR_DISCRIMINATOR, REAP_EXPIRED_DISCRIMINATOR,
        SWAP_DISCRIMINATOR, UPDATE_CONFIG_DISCRIMINATOR, UPDATE_FEE_DISCRIMINATOR,
        WITHDRAW_DISCRIMINATOR,
    };

    use crate::types::Quote;

    pub const MASS_QUOTE_DISCRIMINATOR: u8 = 20;

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct MassQuoteInstructionArgs {
        pub seat_index: u16,
        pub expiry_slot: u32,
        pub bids: Vec<Quote>,
        pub asks: Vec<Quote>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MassQuote {
        pub owner: solana_pubkey::Pubkey,
        pub market: solana_pubkey::Pubkey,
    }

    impl MassQuote {
        pub fn instruction(
            &self,
            args: MassQuoteInstructionArgs,
        ) -> solana_instruction::Instruction {
            let bid_count = u8::try_from(args.bids.len()).expect("at most 255 bid quotes");
            let ask_count = u8::try_from(args.asks.len()).expect("at most 255 ask quotes");
            let mut data = Vec::with_capacity(8 + 9 * (args.bids.len() + args.asks.len()));

            data.push(MASS_QUOTE_DISCRIMINATOR);
            data.extend_from_slice(&args.seat_index.to_le_bytes());
            data.extend_from_slice(&args.expiry_slot.to_le_bytes());
            data.push(bid_count);
            for quote in args.bids {
                encode_quote(&mut data, quote);
            }
            data.push(ask_count);
            for quote in args.asks {
                encode_quote(&mut data, quote);
            }

            solana_instruction::Instruction {
                program_id: crate::CLOB_ID,
                accounts: vec![
                    solana_instruction::AccountMeta::new_readonly(self.owner, true),
                    solana_instruction::AccountMeta::new(self.market, false),
                ],
                data,
            }
        }
    }

    fn encode_quote(data: &mut Vec<u8>, quote: Quote) {
        data.extend_from_slice(&quote.tick.to_le_bytes());
        data.extend_from_slice(&quote.lots.to_le_bytes());
        data.push(quote.flags);
    }
}

pub mod types {
    pub use crate::generated::types::*;
}

pub mod snapshot;
