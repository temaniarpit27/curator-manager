//! The types the rest of the crate works with: deliveries, events, and
//! lines that couldn't be decoded.

use thiserror::Error;

use crate::types::{Assets, BlockNumber, EventId, Shares, StrategyId, UserId, VaultId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Event(Event),
    /// Every event from `from_block` on is no longer canonical.
    Reorg {
        from_block: BlockNumber,
    },
    /// A well-formed event of a kind we don't know.
    Unsupported {
        id: EventId,
        vault: VaultId,
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: EventId,
    pub vault: VaultId,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    VaultCreated { decimals: u8 },
    Deposit { user: UserId, assets: Assets, shares: Shares },
    Withdraw { user: UserId, assets: Assets, shares: Shares },
    Accrue { assets: Assets },
    SetCap { strategy: StrategyId, cap: Assets },
    Allocate { strategy: StrategyId, assets: Assets },
    Deallocate { strategy: StrategyId, assets: Assets },
}

/// A line that could not be decoded into a delivery.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("line {line}: {reason}")]
pub struct ParseError {
    pub line: usize,
    pub scope: Scope,
    pub reason: String,
}

/// What a broken line can still be attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Unknown,
    /// Broken envelope, but it names its vault and a supported kind.
    Vault(VaultId),
    /// Only `data` was missing or bad.
    Event(VaultId, EventId),
}
