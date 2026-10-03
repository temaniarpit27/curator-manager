//! Planning the moves that bring one vault towards its policy.
//!
//! Each strategy's target is `min(floor(total × bps / 10_000), cap)`, where
//! a missing cap counts as zero. Each strategy off target gets one move:
//! deallocations first, then allocations. The moves are simulated on a copy
//! and the result is checked before a [`Plan`] is returned. A plan is advice;
//! nothing here executes it.

use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;
use tracing::debug;

use crate::policy::VaultPolicy;
use crate::types::{Assets, Bps, MathError, StrategyId};
use crate::vault::Vault;

/// One step of a plan. Assets only move through idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    Deallocate { strategy: StrategyId, amount: Assets, reason: DeallocateReason },
    Allocate { strategy: StrategyId, amount: Assets },
}

impl Move {
    pub fn strategy(&self) -> &StrategyId {
        match self {
            Move::Deallocate { strategy, .. } | Move::Allocate { strategy, .. } => strategy,
        }
    }

    pub fn amount(&self) -> Assets {
        match self {
            Move::Deallocate { amount, .. } | Move::Allocate { amount, .. } => *amount,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeallocateReason {
    NotInPolicy,
    AboveCap,
    /// In the policy, but no cap is set on chain, so it can hold nothing.
    NoCap,
    AboveTarget,
}

impl fmt::Display for DeallocateReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotInPolicy => "not in policy",
            Self::AboveCap => "above cap",
            Self::NoCap => "no cap set",
            Self::AboveTarget => "above target",
        })
    }
}

/// What the policy asks of one strategy, and what the plan can deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    strategy: StrategyId,
    bps: Option<Bps>,
    cap: Option<Assets>,
    requested: Assets,
    reachable: Assets,
}

impl Target {
    pub fn strategy(&self) -> &StrategyId {
        &self.strategy
    }

    pub fn bps(&self) -> Option<Bps> {
        self.bps
    }

    pub fn cap(&self) -> Option<Assets> {
        self.cap
    }

    pub fn requested(&self) -> Assets {
        self.requested
    }

    pub fn reachable(&self) -> Assets {
        self.reachable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error(transparent)]
    Math(#[from] MathError),
    #[error("move {index} ({mv:?}) is not executable: {reason}")]
    BadMove { index: usize, mv: Move, reason: StepError },
    #[error("projected state fails validation: {0}")]
    Invalid(Invalid),
    #[error("the vault has no VaultCreated, so it has no state to plan from")]
    NotCreated,
}

/// What the final check found wrong; any of these is a planner bug.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Invalid {
    #[error("assets not conserved: {before} before, {after} after")]
    NotConserved { before: Assets, after: Assets },
    #[error("shares or holder balances changed")]
    SharesChanged,
    #[error("{0} ends above its cap")]
    AboveCap(StrategyId),
    #[error("{strategy} ends at {balance}, target is {target}")]
    OffTarget { strategy: StrategyId, balance: Assets, target: Assets },
    #[error("idle ends at {idle}, expected {expected}")]
    WrongIdle { idle: Assets, expected: Assets },
}

/// A plan that has been simulated and checked; only [`plan`] builds one.
#[derive(Debug, Clone)]
pub struct Plan {
    moves: Vec<Move>,
    targets: Vec<Target>,
    projected: Vault,
}

impl Plan {
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The vault as it would be after every move.
    pub fn projected(&self) -> &Vault {
        &self.projected
    }
}

/// Plan the moves for one vault. Whether its input can be trusted is the
/// caller's check (`run::report`).
pub fn plan(vault: &Vault, policy: &VaultPolicy) -> Result<Plan, PlanError> {
    if !vault.is_created() {
        return Err(PlanError::NotCreated);
    }
    let total = vault.total_assets()?;
    let targets = targets(vault, policy, total)?;

    let mut deallocations = Vec::new();
    let mut allocations = Vec::new();
    for t in &targets {
        let balance = vault.balance_of(&t.strategy);
        if balance > t.reachable {
            // The reason is whichever limit set the target.
            let reason = if t.reachable < t.requested && t.cap.is_none() {
                DeallocateReason::NoCap
            } else if t.reachable < t.requested {
                DeallocateReason::AboveCap
            } else if t.bps.is_none() {
                DeallocateReason::NotInPolicy
            } else {
                DeallocateReason::AboveTarget
            };
            let amount = balance.checked_sub(t.reachable)?;
            deallocations.push(Move::Deallocate { strategy: t.strategy.clone(), amount, reason });
        } else if balance < t.reachable {
            let amount = t.reachable.checked_sub(balance)?;
            allocations.push(Move::Allocate { strategy: t.strategy.clone(), amount });
        }
    }
    // Targets sum to at most the total, so once the deallocations are done
    // idle always covers the allocations.
    let moves: Vec<Move> = deallocations.into_iter().chain(allocations).collect();

    let mut projected = vault.clone();
    for (index, mv) in moves.iter().enumerate() {
        apply_move(&mut projected, mv).map_err(|reason| PlanError::BadMove {
            index,
            mv: mv.clone(),
            reason,
        })?;
        debug!(?mv, "move simulated");
    }
    validate(vault, &projected, &targets)?;
    Ok(Plan { moves, targets, projected })
}

fn targets(vault: &Vault, policy: &VaultPolicy, total: Assets) -> Result<Vec<Target>, PlanError> {
    let names: BTreeSet<&StrategyId> = vault.strategies.keys().chain(policy.strategies()).collect();
    names
        .into_iter()
        .map(|strategy| {
            let bps = policy.get(strategy);
            let cap = vault.strategies.get(strategy).and_then(|s| s.cap);
            let requested = match bps {
                Some(bps) => bps.of(total)?,
                None => Assets::ZERO,
            };
            let reachable = requested.min(cap.unwrap_or(Assets::ZERO));
            Ok(Target { strategy: strategy.clone(), bps, cap, requested, reachable })
        })
        .collect()
}

/// Run one move on the simulated vault: the source must hold the amount and
/// an allocation must stay within its cap.
fn apply_move(vault: &mut Vault, mv: &Move) -> Result<(), StepError> {
    let mut s = vault.strategies.get(mv.strategy()).cloned().unwrap_or_default();
    let amount = mv.amount();
    let idle = match mv {
        Move::Deallocate { .. } => {
            s.balance = s.balance.checked_sub(amount)?;
            vault.idle.checked_add(amount)?
        }
        Move::Allocate { .. } => {
            let idle = vault.idle.checked_sub(amount)?;
            s.balance = s.balance.checked_add(amount)?;
            if s.is_over_cap() {
                return Err(StepError::OverCap { balance: s.balance, cap: s.effective_cap() });
            }
            idle
        }
    };
    vault.idle = idle;
    vault.strategies.insert(mv.strategy().clone(), s);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StepError {
    #[error(transparent)]
    Math(#[from] MathError),
    #[error("would hold {balance}, cap is {cap}")]
    OverCap { balance: Assets, cap: Assets },
}

/// Check the end result as a whole, including strategies no move touched.
fn validate(before: &Vault, after: &Vault, targets: &[Target]) -> Result<(), PlanError> {
    let invalid = |problem| Err(PlanError::Invalid(problem));
    let total = before.total_assets()?;
    let after_total = after.total_assets()?;
    if after_total != total {
        return invalid(Invalid::NotConserved { before: total, after: after_total });
    }
    if after.total_shares != before.total_shares || after.holders != before.holders {
        return invalid(Invalid::SharesChanged);
    }
    if let Some((name, _)) = after.strategies.iter().find(|(_, s)| s.is_over_cap()) {
        return invalid(Invalid::AboveCap(name.clone()));
    }
    for t in targets {
        let balance = after.balance_of(&t.strategy);
        if balance != t.reachable {
            let strategy = t.strategy.clone();
            return invalid(Invalid::OffTarget { strategy, balance, target: t.reachable });
        }
    }
    let assigned = Assets::checked_sum(targets.iter().map(|t| t.reachable))?;
    let expected = total.checked_sub(assigned)?;
    if after.idle != expected {
        return invalid(Invalid::WrongIdle { idle: after.idle, expected });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::StrategyState;
    use std::collections::BTreeMap;

    fn plan_for(vault: &Vault, policy: &VaultPolicy) -> Result<Plan, PlanError> {
        plan(vault, policy)
    }

    fn vault(idle: u128, strategies: &[(&str, u128, Option<u128>)]) -> Vault {
        Vault {
            decimals: Some(6),
            idle: Assets::new(idle),
            strategies: strategies
                .iter()
                .map(|(n, b, c)| {
                    (
                        (*n).into(),
                        StrategyState { balance: Assets::new(*b), cap: c.map(Assets::new) },
                    )
                })
                .collect(),
            ..Vault::default()
        }
    }

    fn policy(entries: &[(&str, u64)]) -> VaultPolicy {
        let targets: BTreeMap<StrategyId, Bps> =
            entries.iter().map(|(n, b)| ((*n).into(), Bps::try_from(*b).unwrap())).collect();
        VaultPolicy::try_from(targets).unwrap()
    }

    fn dealloc(strategy: &str, amount: u128, reason: DeallocateReason) -> Move {
        Move::Deallocate { strategy: strategy.into(), amount: Assets::new(amount), reason }
    }

    fn alloc(strategy: &str, amount: u128) -> Move {
        Move::Allocate { strategy: strategy.into(), amount: Assets::new(amount) }
    }

    #[test]
    fn move_count_is_the_number_of_strategies_off_target() {
        // Total 800, so each 20% target is 160. a: above target, b: below,
        // c: on target (so no move), d: over its cap of 100, e: not in the policy.
        let v = vault(
            140,
            &[
                ("a", 300, Some(1_000)),
                ("b", 0, Some(1_000)),
                ("c", 160, Some(1_000)),
                ("d", 150, Some(100)),
                ("e", 50, Some(1_000)),
            ],
        );
        let p = policy(&[("a", 2_000), ("b", 2_000), ("c", 2_000), ("d", 2_000)]);
        let plan = plan_for(&v, &p).unwrap();
        let off_target = plan
            .targets()
            .iter()
            .filter(|t| {
                v.strategies.get(&t.strategy).map_or(Assets::ZERO, |s| s.balance) != t.reachable
            })
            .count();
        assert_eq!((plan.moves().len(), off_target), (4, 4), "c is on target and gets no move");
        // One net move each: a goes 300 -> 160 in a single step, never drained
        // to idle and refilled.
        assert!(plan.moves().contains(&dealloc("a", 140, DeallocateReason::AboveTarget)));
        assert!(plan.moves().contains(&dealloc("e", 50, DeallocateReason::NotInPolicy)));
    }

    #[test]
    fn an_empty_vault_plans_nothing() {
        let v = vault(0, &[("s", 0, Some(100))]);
        let plan = plan_for(&v, &policy(&[("s", 10_000)])).unwrap();
        assert!(plan.moves().is_empty());
        assert_eq!(plan.targets()[0].requested, Assets::ZERO);
    }

    #[test]
    fn deallocation_reason_names_the_limit_that_sets_the_amount() {
        // Over its cap of 100, but the 50 target is lower: the target sets it.
        let v = vault(0, &[("s", 150, Some(100))]);
        let plan = plan_for(&v, &policy(&[("s", 3_334)])).unwrap();
        assert_eq!(plan.moves(), [dealloc("s", 100, DeallocateReason::AboveTarget)]);
        // Not in the policy and over its cap: the zero target sets it.
        let v = vault(0, &[("s", 150, Some(100))]);
        let plan = plan_for(&v, &policy(&[])).unwrap();
        assert_eq!(plan.moves(), [dealloc("s", 150, DeallocateReason::NotInPolicy)]);
        // The 100% target is higher than the cap of 100: the cap sets it.
        let v = vault(0, &[("s", 150, Some(100))]);
        let plan = plan_for(&v, &policy(&[("s", 10_000)])).unwrap();
        assert_eq!(plan.moves(), [dealloc("s", 50, DeallocateReason::AboveCap)]);
        // In the policy but with no cap on chain: it can hold nothing.
        let v = vault(0, &[("s", 50, None)]);
        let plan = plan_for(&v, &policy(&[("s", 10_000)])).unwrap();
        assert_eq!(plan.moves(), [dealloc("s", 50, DeallocateReason::NoCap)]);
    }

    #[test]
    fn apply_move_refuses_unsafe_moves() {
        let mut v = vault(50, &[("s", 10, Some(30))]);
        assert!(apply_move(&mut v, &alloc("s", 25)).is_err(), "over cap");
        assert!(
            apply_move(&mut v, &dealloc("s", 11, DeallocateReason::AboveTarget)).is_err(),
            "overdraw"
        );
        assert!(apply_move(&mut v, &alloc("new", 1)).is_err(), "no cap");
        assert_eq!(v, vault(50, &[("s", 10, Some(30))]), "failed moves change nothing");
    }

    #[test]
    fn plan_refuses_an_uncreated_vault() {
        let uncreated = Vault::default();
        assert_eq!(plan_for(&uncreated, &policy(&[])).err(), Some(PlanError::NotCreated));
    }

    #[test]
    fn validate_catches_planner_bugs() {
        let before = vault(0, &[("s", 80, Some(60))]);
        let target = Target {
            strategy: "s".into(),
            bps: None,
            cap: Some(Assets::new(60)),
            requested: Assets::new(0),
            reachable: Assets::new(0),
        };
        // A buggy planner that left the over-cap strategy alone.
        assert!(matches!(validate(&before, &before, &[target]), Err(PlanError::Invalid(_))));
    }
}
