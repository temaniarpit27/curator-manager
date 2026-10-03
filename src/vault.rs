//! Accounting: one vault's state, and how a single event changes it.
//!
//! [`Vault::apply`] is all or nothing: every new value is worked out first,
//! and only assigned if every check passes. A failure means the event can't
//! be applied yet, for example no `VaultCreated` or not enough idle.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::event::{Event, EventKind};
use crate::types::{Assets, EventId, MathError, Shares, StrategyId, UserId, mul_div_floor};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrategyState {
    pub balance: Assets,
    /// `None` until a `SetCap` is seen.
    pub cap: Option<Assets>,
}

impl StrategyState {
    /// No cap set counts as zero: such a strategy can never be funded.
    pub fn effective_cap(&self) -> Assets {
        self.cap.unwrap_or(Assets::ZERO)
    }

    pub fn is_over_cap(&self) -> bool {
        self.balance > self.effective_cap()
    }
}

/// Facts noticed while applying the current chain; rebuilt on every replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observations {
    /// Later `VaultCreated` events with the same decimals (harmless).
    pub repeated_creations: Vec<EventId>,
    /// Later `VaultCreated` events with different decimals (holds the plan).
    pub conflicting_creations: Vec<EventId>,
    /// Per strategy, the first allocation that went above its cap.
    pub cap_breaches: BTreeMap<StrategyId, EventId>,
    /// Deposits or withdrawals where the user gave something and got
    /// nothing back, which rounding in the vault's favour explains.
    pub rounded_flows: Vec<EventId>,
    /// Deposits or withdrawals where the user got something for nothing
    /// (holds the plan: rounding can't explain it).
    pub unbacked_flows: Vec<EventId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vault {
    /// Set by the first `VaultCreated`; `None` means not created yet.
    pub decimals: Option<u8>,
    pub idle: Assets,
    pub total_shares: Shares,
    pub holders: BTreeMap<UserId, Shares>,
    pub strategies: BTreeMap<StrategyId, StrategyState>,
    pub observations: Observations,
}

/// Why an event can't be applied. Each message includes the arithmetic
/// detail, so no variant also exposes it as a `source`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    #[error("no VaultCreated earlier in chain order")]
    MissingCreation,
    #[error("idle: {0}")]
    Idle(MathError),
    #[error("strategy {strategy}: {error}")]
    Strategy { strategy: StrategyId, error: MathError },
    #[error("holder {user}: {error}")]
    Holder { user: UserId, error: MathError },
    #[error("total shares: {0}")]
    TotalShares(MathError),
    #[error("total assets: {0}")]
    TotalAssets(MathError),
}

impl Vault {
    pub fn is_created(&self) -> bool {
        self.decimals.is_some()
    }

    /// What a strategy holds; zero if the vault has never seen it.
    pub fn balance_of(&self, strategy: &StrategyId) -> Assets {
        self.strategies.get(strategy).map_or(Assets::ZERO, |s| s.balance)
    }

    /// Idle plus every strategy balance.
    pub fn total_assets(&self) -> Result<Assets, MathError> {
        let balances = self.strategies.values().map(|s| s.balance);
        Assets::checked_sum(std::iter::once(self.idle).chain(balances))
    }

    /// `floor(shares * total_assets / total_shares)`; `None` with no shares.
    pub fn value_of(&self, shares: Shares) -> Result<Option<Assets>, MathError> {
        if self.total_shares == Shares::ZERO {
            return Ok(None);
        }
        let total = self.total_assets()?;
        mul_div_floor(shares.get(), total.get(), self.total_shares.get())
            .map(|v| Some(Assets::new(v)))
    }

    /// Apply one event, all or nothing.
    pub fn apply(&mut self, event: &Event) -> Result<(), ApplyError> {
        if !self.is_created() && !matches!(event.kind, EventKind::VaultCreated { .. }) {
            return Err(ApplyError::MissingCreation);
        }
        match &event.kind {
            // The first creation stands; later ones are only recorded.
            EventKind::VaultCreated { decimals } => match self.decimals {
                None => self.decimals = Some(*decimals),
                Some(d) if d == *decimals => self.observations.repeated_creations.push(event.id),
                Some(_) => self.observations.conflicting_creations.push(event.id),
            },
            EventKind::Deposit { user, assets, shares } => {
                self.check_total_can_grow(*assets)?;
                let idle = self.idle.checked_add(*assets).map_err(ApplyError::Idle)?;
                let held = self.held_by(user).checked_add(*shares).map_err(|e| holder(user, e))?;
                let total =
                    self.total_shares.checked_add(*shares).map_err(ApplyError::TotalShares)?;
                self.idle = idle;
                self.set_holder(user, held);
                self.total_shares = total;
                self.note_one_sided(event.id, shares.get(), assets.get());
            }
            EventKind::Withdraw { user, assets, shares } => {
                let idle = self.idle.checked_sub(*assets).map_err(ApplyError::Idle)?;
                let held = self.held_by(user).checked_sub(*shares).map_err(|e| holder(user, e))?;
                let total =
                    self.total_shares.checked_sub(*shares).map_err(ApplyError::TotalShares)?;
                self.idle = idle;
                self.set_holder(user, held);
                self.total_shares = total;
                self.note_one_sided(event.id, assets.get(), shares.get());
            }
            EventKind::Accrue { assets } => {
                self.check_total_can_grow(*assets)?;
                self.idle = self.idle.checked_add(*assets).map_err(ApplyError::Idle)?;
            }
            EventKind::SetCap { strategy, cap } => {
                self.strategies.entry(strategy.clone()).or_default().cap = Some(*cap);
            }
            EventKind::Allocate { strategy, assets } => {
                let idle = self.idle.checked_sub(*assets).map_err(ApplyError::Idle)?;
                let balance = self
                    .balance_of(strategy)
                    .checked_add(*assets)
                    .map_err(|e| strategy_error(strategy, e))?;
                self.idle = idle;
                let s = self.strategies.entry(strategy.clone()).or_default();
                s.balance = balance;
                // History is applied as reported; going over the cap is
                // only recorded.
                if s.is_over_cap() {
                    let breaches = &mut self.observations.cap_breaches;
                    breaches.entry(strategy.clone()).or_insert(event.id);
                }
            }
            EventKind::Deallocate { strategy, assets } => {
                let balance = self
                    .balance_of(strategy)
                    .checked_sub(*assets)
                    .map_err(|e| strategy_error(strategy, e))?;
                let idle = self.idle.checked_add(*assets).map_err(ApplyError::Idle)?;
                self.idle = idle;
                self.strategies.entry(strategy.clone()).or_default().balance = balance;
            }
        }
        Ok(())
    }

    /// Record a deposit or withdrawal with one side zero; it is still applied
    /// as reported. A vault rounds in its own favour, so a user can get
    /// nothing for what they give (a tiny deposit mints 0 shares, a tiny
    /// redemption pays 0 assets). Getting something for nothing can't come
    /// from rounding.
    fn note_one_sided(&mut self, id: EventId, got: u128, gave: u128) {
        match (got == 0, gave == 0) {
            (true, false) => self.observations.rounded_flows.push(id),
            (false, true) => self.observations.unbacked_flows.push(id),
            _ => {}
        }
    }

    fn set_holder(&mut self, user: &UserId, held: Shares) {
        if held == Shares::ZERO {
            self.holders.remove(user);
        } else {
            self.holders.insert(user.clone(), held);
        }
    }

    /// Only deposits and accruals grow the total, so checking here keeps
    /// `total_assets()` from ever failing.
    fn check_total_can_grow(&self, by: Assets) -> Result<(), ApplyError> {
        self.total_assets()
            .and_then(|t| t.checked_add(by))
            .map(|_| ())
            .map_err(ApplyError::TotalAssets)
    }

    fn held_by(&self, user: &UserId) -> Shares {
        self.holders.get(user).copied().unwrap_or(Shares::ZERO)
    }
}

fn holder(user: &UserId, error: MathError) -> ApplyError {
    ApplyError::Holder { user: user.clone(), error }
}

fn strategy_error(strategy: &StrategyId, error: MathError) -> ApplyError {
    ApplyError::Strategy { strategy: strategy.clone(), error }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(block: u64, kind: EventKind) -> Event {
        Event { id: EventId { block, log_index: 0 }, vault: "v".into(), kind }
    }

    fn created() -> Event {
        ev(0, EventKind::VaultCreated { decimals: 6 })
    }

    fn deposit(block: u64, user: &str, assets: u128, shares: u128) -> Event {
        ev(
            block,
            EventKind::Deposit {
                user: user.into(),
                assets: Assets::new(assets),
                shares: Shares::new(shares),
            },
        )
    }

    fn set_cap(block: u64, cap: u128) -> Event {
        ev(block, EventKind::SetCap { strategy: "s".into(), cap: Assets::new(cap) })
    }

    fn allocate(block: u64, assets: u128) -> Event {
        ev(block, EventKind::Allocate { strategy: "s".into(), assets: Assets::new(assets) })
    }

    /// Apply events in order onto a created vault.
    fn apply_all(events: &[Event]) -> (Vault, Result<(), ApplyError>) {
        let mut v = Vault::default();
        let result = std::iter::once(&created()).chain(events).try_for_each(|e| v.apply(e));
        (v, result)
    }

    #[test]
    fn a_failed_event_leaves_the_vault_untouched() {
        let (mut v, _) = apply_all(&[deposit(1, "a", 100, 100)]);
        let before = v.clone();
        let over_burn = ev(
            2,
            EventKind::Withdraw {
                user: "a".into(),
                assets: Assets::new(10),
                shares: Shares::new(150),
            },
        );
        assert!(matches!(v.apply(&over_burn), Err(ApplyError::Holder { .. })));
        assert!(matches!(v.apply(&allocate(3, 500)), Err(ApplyError::Idle(_))));
        let dealloc = ev(4, EventKind::Deallocate { strategy: "s".into(), assets: Assets::new(1) });
        assert!(matches!(v.apply(&dealloc), Err(ApplyError::Strategy { .. })));
        assert_eq!(v, before);
    }

    #[test]
    fn nothing_but_creation_applies_before_creation() {
        let mut v = Vault::default();
        assert_eq!(v.apply(&deposit(1, "frank", 1_000, 1_000)), Err(ApplyError::MissingCreation));
        assert_eq!(v, Vault::default());
        assert_eq!(v.apply(&created()), Ok(()));
        assert_eq!(v.apply(&deposit(1, "frank", 1_000, 1_000)), Ok(()));
    }

    #[test]
    fn repeated_creation_is_observed_and_changes_nothing() {
        let same = ev(2, EventKind::VaultCreated { decimals: 6 });
        let different = ev(3, EventKind::VaultCreated { decimals: 18 });
        let (v, r) = apply_all(&[same, different]);
        assert_eq!(r, Ok(()));
        assert_eq!(v.decimals, Some(6), "the first creation stands");
        assert_eq!(v.observations.repeated_creations, vec![EventId { block: 2, log_index: 0 }]);
        assert_eq!(v.observations.conflicting_creations, vec![EventId { block: 3, log_index: 0 }]);
    }

    #[test]
    fn one_sided_flows_are_applied_and_sorted_by_whether_rounding_explains_them() {
        let withdraw = |block, assets, shares| {
            ev(
                block,
                EventKind::Withdraw {
                    user: "m".into(),
                    assets: Assets::new(assets),
                    shares: Shares::new(shares),
                },
            )
        };
        let id = |block| EventId { block, log_index: 0 };
        let (v, r) = apply_all(&[
            deposit(2, "a", 100, 100),
            // Something for nothing: assets out with no shares burned (by
            // someone holding none), and shares minted for no assets.
            withdraw(3, 90, 0),
            deposit(4, "b", 0, 50),
            // Rounding in the vault's favour: a share redeemed for nothing,
            // and an asset deposited for no shares.
            ev(
                5,
                EventKind::Withdraw {
                    user: "a".into(),
                    assets: Assets::ZERO,
                    shares: Shares::new(1),
                },
            ),
            deposit(6, "c", 1, 0),
            // Both sides zero changes nothing.
            deposit(7, "d", 0, 0),
        ]);
        assert_eq!(r, Ok(()), "applied as reported");
        assert_eq!(v.idle, Assets::new(11));
        assert_eq!(v.observations.unbacked_flows, [id(3), id(4)]);
        assert_eq!(v.observations.rounded_flows, [id(5), id(6)]);
    }

    #[test]
    fn lowered_cap_is_valid_and_over_cap() {
        // Lowering a cap below the balance is fine on chain; it's flagged.
        let (v, r) = apply_all(&[
            deposit(1, "a", 100, 100),
            set_cap(2, 80),
            allocate(3, 80),
            set_cap(4, 50),
        ]);
        assert_eq!(r, Ok(()));
        assert!(v.strategies["s"].is_over_cap());
        assert!(v.observations.cap_breaches.is_empty(), "the allocation itself respected the cap");
    }

    #[test]
    fn allocation_above_cap_at_the_time_is_applied_and_observed() {
        let (v, r) = apply_all(&[deposit(1, "a", 100, 100), set_cap(2, 10), allocate(3, 40)]);
        assert_eq!(r, Ok(()));
        assert_eq!(v.strategies["s"].balance, Assets::new(40));
        assert_eq!(v.observations.cap_breaches["s"], EventId { block: 3, log_index: 0 });
    }

    #[test]
    fn aggregate_total_overflow_fails_even_when_each_field_fits() {
        let (v, r) = apply_all(&[
            set_cap(1, u128::MAX),
            deposit(2, "a", u128::MAX, 1),
            allocate(3, u128::MAX),
            ev(4, EventKind::Accrue { assets: Assets::new(1) }),
        ]);
        assert!(matches!(r, Err(ApplyError::TotalAssets(_))));
        assert_eq!(v.idle, Assets::ZERO, "the overflowing accrue was not applied");
        assert!(v.total_assets().is_ok());
    }

    #[test]
    fn share_ledger_reconciles_after_every_kind_of_event() {
        let mut v = Vault::default();
        let events = [
            created(),
            deposit(1, "a", 100, 90),
            deposit(2, "b", 50, 40),
            ev(3, EventKind::Accrue { assets: Assets::new(10) }),
            set_cap(4, 1_000),
            allocate(5, 60),
            ev(6, EventKind::Deallocate { strategy: "s".into(), assets: Assets::new(20) }),
            ev(
                7,
                EventKind::Withdraw {
                    user: "a".into(),
                    assets: Assets::new(30),
                    shares: Shares::new(27),
                },
            ),
            ev(
                8,
                EventKind::Withdraw {
                    user: "b".into(),
                    assets: Assets::new(10),
                    shares: Shares::new(40),
                },
            ),
        ];
        for e in &events {
            let before = v.clone();
            v.apply(e).unwrap();
            // Holder shares and total supply always change together.
            let held = Shares::checked_sum(v.holders.values().copied());
            assert_eq!(held, Ok(v.total_shares), "after {:?}", e.kind);
            // Only a deposit or withdrawal moves shares, and only for its user.
            let moved: Vec<_> =
                v.holders.iter().filter(|(u, s)| before.holders.get(*u) != Some(s)).collect();
            match &e.kind {
                EventKind::Deposit { user, shares, .. }
                | EventKind::Withdraw { user, shares, .. } => {
                    assert!(moved.iter().all(|(u, _)| *u == user));
                    assert_eq!(
                        v.total_shares.get().abs_diff(before.total_shares.get()),
                        shares.get()
                    );
                }
                _ => assert!(moved.is_empty() && v.total_shares == before.total_shares),
            }
        }
        assert!(!v.holders.contains_key("b"), "a full withdrawal removes the holder");
        // A zero-share withdrawal by an unknown user creates no phantom holder.
        let nothing = Shares::new(0);
        let ghost =
            EventKind::Withdraw { user: "z".into(), assets: Assets::new(0), shares: nothing };
        v.apply(&ev(9, ghost)).unwrap();
        assert!(!v.holders.contains_key("z"));
    }

    #[test]
    fn holder_value_floors_and_is_undefined_without_shares() {
        let (v, _) = apply_all(&[ev(1, EventKind::Accrue { assets: Assets::new(10) })]);
        assert_eq!(v.value_of(Shares::new(0)), Ok(None));
        let (v, _) = apply_all(&[deposit(1, "a", 10, 3)]);
        assert_eq!(v.value_of(Shares::new(1)), Ok(Some(Assets::new(3))));
    }
}
