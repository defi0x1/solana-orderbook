//! Canonical read-only decoder for the complete market account.

use crate::{
    accounts::Market,
    types::{Level, OrderNode, Seat},
};
use borsh::BorshDeserialize;

pub const MARKET_VERSION: u8 = 4;
pub const HEADER_LEN: usize = 320;
pub const WINDOW_TICKS: usize = 8_192;
pub const WINDOW_MASK: u32 = WINDOW_TICKS as u32 - 1;
pub const BITMAP_WORDS: usize = WINDOW_TICKS / 64;
pub const BITMAP_LEN: usize = (2 + BITMAP_WORDS) * 8;
pub const LEVEL_LEN: usize = 16;
pub const SEAT_COUNT: usize = 128;
pub const SEAT_LEN: usize = 80;
pub const NODE_LEN: usize = 30;
pub const FREED_SEQ: u64 = u64::MAX;

pub const BID_BITMAP_OFFSET: usize = HEADER_LEN;
pub const ASK_BITMAP_OFFSET: usize = BID_BITMAP_OFFSET + BITMAP_LEN;
pub const LEVELS_OFFSET: usize = ASK_BITMAP_OFFSET + BITMAP_LEN;
pub const SEATS_OFFSET: usize = LEVELS_OFFSET + WINDOW_TICKS * LEVEL_LEN;
pub const POOL_OFFSET: usize = SEATS_OFFSET + SEAT_COUNT * SEAT_LEN;

const MAKER_INDEX_MASK: u16 = 0x7FFF;
const ASK_SIDE_FLAG: u16 = 0x8000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderSide {
    Bid,
    Ask,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitmapSnapshot {
    pub summary: [u64; 2],
    pub leaves: Vec<u64>,
}

impl BitmapSnapshot {
    pub fn contains(&self, tick: u32) -> bool {
        let bit = tick & WINDOW_MASK;
        self.leaves[(bit >> 6) as usize] & (1u64 << (bit & 63)) != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketSnapshot {
    pub header: Market,
    pub bid_bitmap: BitmapSnapshot,
    pub ask_bitmap: BitmapSnapshot,
    pub levels: Vec<Level>,
    pub seats: Vec<Seat>,
    /// Pool index is the vector index; index 0 is the reserved NIL node.
    pub orders: Vec<OrderNode>,
}

impl MarketSnapshot {
    pub fn decode(data: &[u8]) -> Result<Self, SnapshotError> {
        if data.len() < POOL_OFFSET {
            return Err(SnapshotError::TooShort {
                actual: data.len(),
                minimum: POOL_OFFSET,
            });
        }

        let header = Market::from_bytes(&data[..HEADER_LEN]).map_err(SnapshotError::Decode)?;
        if header.version != MARKET_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                actual: header.version,
                expected: MARKET_VERSION,
            });
        }
        let expected = POOL_OFFSET
            .checked_add(header.order_pool_capacity as usize * NODE_LEN)
            .ok_or(SnapshotError::LengthOverflow)?;
        if data.len() != expected {
            return Err(SnapshotError::LengthMismatch {
                actual: data.len(),
                expected,
            });
        }

        Ok(Self {
            header,
            bid_bitmap: decode_bitmap(&data[BID_BITMAP_OFFSET..ASK_BITMAP_OFFSET]),
            ask_bitmap: decode_bitmap(&data[ASK_BITMAP_OFFSET..LEVELS_OFFSET]),
            levels: decode_fixed(&data[LEVELS_OFFSET..SEATS_OFFSET], LEVEL_LEN, WINDOW_TICKS)?,
            seats: decode_fixed(&data[SEATS_OFFSET..POOL_OFFSET], SEAT_LEN, SEAT_COUNT)?,
            orders: decode_fixed(
                &data[POOL_OFFSET..],
                NODE_LEN,
                (data.len() - POOL_OFFSET) / NODE_LEN,
            )?,
        })
    }

    pub fn live_orders(&self) -> impl Iterator<Item = LiveOrder<'_>> {
        self.orders
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, node)| node.seq != FREED_SEQ)
            .map(|(index, node)| LiveOrder {
                index: index as u16,
                maker: node.maker_and_side & MAKER_INDEX_MASK,
                side: if node.maker_and_side & ASK_SIDE_FLAG == 0 {
                    OrderSide::Bid
                } else {
                    OrderSide::Ask
                },
                node,
            })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LiveOrder<'a> {
    pub index: u16,
    pub maker: u16,
    pub side: OrderSide,
    pub node: &'a OrderNode,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("market account is too short: {actual} bytes, need at least {minimum}")]
    TooShort { actual: usize, minimum: usize },
    #[error("unsupported market version {actual}, expected {expected}")]
    UnsupportedVersion { actual: u8, expected: u8 },
    #[error("market account length {actual} does not match header-derived length {expected}")]
    LengthMismatch { actual: usize, expected: usize },
    #[error("market account length calculation overflowed")]
    LengthOverflow,
    #[error("failed to decode market state: {0}")]
    Decode(std::io::Error),
}

fn decode_bitmap(data: &[u8]) -> BitmapSnapshot {
    let word = |index: usize| {
        let start = index * 8;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&data[start..start + 8]);
        u64::from_le_bytes(bytes)
    };
    BitmapSnapshot {
        summary: [word(0), word(1)],
        leaves: (0..BITMAP_WORDS).map(|index| word(index + 2)).collect(),
    }
}

fn decode_fixed<T: BorshDeserialize>(
    data: &[u8],
    width: usize,
    count: usize,
) -> Result<Vec<T>, SnapshotError> {
    data.chunks_exact(width)
        .take(count)
        .map(|chunk| {
            let mut input = chunk;
            T::deserialize(&mut input).map_err(SnapshotError::Decode)
        })
        .collect()
}
