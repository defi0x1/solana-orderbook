use pinocchio::program_error::ProgramError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub(crate) enum ClobError {
    // 0
    /// Account expected to be mutable
    NotMutable,
    /// Account expected to be a signer
    NotSigner,
    /// Account expected to be owned by our program
    InvalidAccountOwner,
    /// The Account data length is not the expected one
    InvalidAccountLength,
    /// The Market version is not the expected one
    InvalidVersion,

    // 5
    /// The seat index is out of range
    InvalidSeatIndex,
    /// The seat is not owned by the signer
    InvalidSeatOwner,
    /// The seat table is full
    SeatTableFull,
    /// The seat still has balances or resting orders
    SeatNotEmpty,
    /// The tick is outside the current book window
    TickOutOfWindow,

    // 10
    /// The lots amount is zero or above the per-order maximum
    InvalidLots,
    /// The order pool is full
    PoolFull,
    /// The order was not found (already filled, cancelled, or reused slot)
    OrderNotFound,
    /// The order would cross the opposing side
    WouldCross,
    /// The mass quote payload is not sorted ascending by tick per side
    PayloadNotSorted,

    // 15
    /// The seat has reached the maximum number of resting orders
    TooManyOrders,
    /// Slippage tolerance exceeded - output amount below minimum threshold
    SlippageExceeded,
    /// The order would self trade and the policy is abort
    SelfTrade,
    /// The Fee is greater than the maximum allowed
    InvalidFee,
    /// The market parameters are invalid or would overflow u64 math
    InvalidMarketParams,

    // 20
    /// The seat balance is insufficient
    InsufficientBalance,
    /// The token account address is not the expected one
    InvalidTokenAddress,
    /// The Program authority is not the expected one
    InvalidProgramAuthority,
    /// The instruction is not valid on this account state
    InvalidState,
    /// A delegated authority pubkey may not be the zero address
    ZeroAuthority,
    /// A mint's owner is neither the legacy Token program nor Token-2022
    UnsupportedTokenProgram,
    /// A mint carries the Token-2022 PermanentDelegate extension, which lets
    /// its delegate move funds out of any holder's account — including this
    /// program's vaults — bypassing every check this program makes
    PermanentDelegateNotAllowed,
}

impl From<ClobError> for ProgramError {
    fn from(e: ClobError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
