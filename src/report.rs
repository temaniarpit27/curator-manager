//! The report: each vault's state, whether it was planned and why not, and
//! how it is printed. Amounts use the vault's decimals; shares are raw.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use thiserror::Error;

use crate::engine::{InputIssue, Warning};
use crate::planner::{Move, Plan, PlanError};
use crate::policy::PolicyProblem;
use crate::types::{
    Assets, EventId, MathError, Shares, UserId, VaultId, format_units, mul_div_floor,
};
use crate::vault::{ApplyError, Vault};

#[derive(Debug)]
pub enum Outcome {
    Planned(Box<Plan>),
    /// Every reason there is no plan (never empty).
    Withheld(Vec<WithheldReason>),
}

/// Why a vault gets no plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WithheldReason {
    #[error("the policy file was rejected, so no vault is planned")]
    PolicyRejected,
    #[error("unresolved: no VaultCreated received for this vault")]
    NeverCreated,
    #[error("unresolved at event {event}: {error}")]
    Unresolved { event: EventId, error: ApplyError },
    #[error(
        "the input for this vault is ambiguous or incomplete; its state is shown for diagnosis only"
    )]
    UntrustedInput(Vec<InputIssue>),
    #[error(
        "no entry in the policy: nothing is planned (an explicit {{}} entry means move everything to idle)"
    )]
    NoPolicyEntry,
    #[error("VaultCreated delivered again with different decimals at {}; which is right is unknown", list(.0))]
    ConflictingCreation(Vec<EventId>),
    #[error(
        "a deposit or withdrawal at {} gave assets or shares for nothing, which rounding can't explain; holder values can't be trusted",
        list(.0)
    )]
    UnbackedFlow(Vec<EventId>),
    #[error("planner refused: {0}")]
    PlannerRefused(PlanError),
}

fn list(ids: &[EventId]) -> String {
    ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

#[derive(Debug)]
pub struct Holding {
    pub user: UserId,
    pub shares: Shares,
    /// `Ok(None)` when no shares exist.
    pub value: Result<Option<Assets>, MathError>,
}

#[derive(Debug)]
pub struct VaultReport {
    pub state: Vault,
    pub holdings: Vec<Holding>,
    /// Total minus holder values: rounding dust, or everything with no shares.
    pub unattributed: Result<Assets, MathError>,
    pub notes: Vec<String>,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub struct Report {
    pub warnings: Vec<Warning>,
    pub policy_errors: Vec<PolicyProblem>,
    pub vaults: BTreeMap<VaultId, VaultReport>,
}

impl Display for Report {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if !self.warnings.is_empty() {
            writeln!(
                f,
                "warnings (deliveries not applied as received; see each vault for any plan withheld):"
            )?;
            for w in &self.warnings {
                writeln!(f, "  ~ {w}")?;
            }
        }
        if !self.policy_errors.is_empty() {
            writeln!(f, "policy rejected (no vault is planned):")?;
            for e in &self.policy_errors {
                writeln!(f, "  ! {e}")?;
            }
        }

        let mut blank = !self.warnings.is_empty() || !self.policy_errors.is_empty();
        for (name, vault) in &self.vaults {
            if blank {
                writeln!(f)?;
            }
            blank = true;
            write_vault(f, name, vault)?;
        }
        Ok(())
    }
}

fn write_vault(f: &mut Formatter<'_>, name: &VaultId, r: &VaultReport) -> fmt::Result {
    let status = match r.outcome {
        Outcome::Planned(_) => "planned",
        Outcome::Withheld(_) => "NOT PLANNED",
    };
    writeln!(f, "== Vault {name} [{status}]")?;
    if let Outcome::Withheld(reasons) = &r.outcome {
        for reason in reasons {
            writeln!(f, "  ! {reason}")?;
            if let WithheldReason::UntrustedInput(issues) = reason {
                for issue in issues {
                    writeln!(f, "    - {issue}")?;
                }
            }
        }
    }
    for note in &r.notes {
        writeln!(f, "  ~ {note}")?;
    }

    // Never created: the balances are unknown, not zero.
    if !r.state.is_created() {
        return writeln!(f, "  current state: not derived (no VaultCreated applied)");
    }
    let d = r.state.decimals;
    // A stuck vault's state stops at the event it couldn't apply.
    let stuck_at = match &r.outcome {
        Outcome::Withheld(reasons) => reasons.iter().find_map(|reason| match reason {
            WithheldReason::Unresolved { event, .. } => Some(*event),
            _ => None,
        }),
        Outcome::Planned(_) => None,
    };
    match stuck_at {
        Some(event) => {
            writeln!(f, "  state so far (stopped at event {event}; later events not applied):")?
        }
        None => writeln!(f, "  current state:")?,
    }
    write_balances(f, &r.state, d, View::Current)?;
    write_holders(f, r, d)?;
    if let Outcome::Planned(plan) = &r.outcome {
        write_plan(f, plan, d)?;
    }
    Ok(())
}

/// The current state (with shares and caps) or a plan's result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Current,
    Projected,
}

fn write_balances(f: &mut Formatter<'_>, v: &Vault, d: Option<u8>, view: View) -> fmt::Result {
    let total = match v.total_assets() {
        Ok(t) => t,
        Err(e) => return writeln!(f, "    total assets: error ({e})"),
    };
    writeln!(f, "    total assets   {}", amt(total, d))?;
    writeln!(f, "    idle           {} ({})", amt(v.idle, d), pct(v.idle, total))?;
    if view == View::Current {
        writeln!(f, "    shares (raw)   {}", v.total_shares)?;
    }
    for (name, s) in &v.strategies {
        let share = format!("{} ({})", amt(s.balance, d), pct(s.balance, total));
        if view == View::Current {
            let cap = s.cap.map_or("none".to_string(), |c| amt(c, d));
            let flag = if s.is_over_cap() { "  ! OVER CAP" } else { "" };
            writeln!(f, "    {name:<14} {share}, cap {cap}{flag}")?;
        } else {
            writeln!(f, "    {name:<14} {share}")?;
        }
    }
    Ok(())
}

fn write_holders(f: &mut Formatter<'_>, r: &VaultReport, d: Option<u8>) -> fmt::Result {
    writeln!(f, "  holders:")?;
    if r.holdings.is_empty() {
        writeln!(f, "    (none)")?;
    }
    for h in &r.holdings {
        let value = match &h.value {
            Ok(Some(v)) => amt(*v, d),
            Ok(None) => "(undefined: no shares outstanding)".into(),
            Err(e) => format!("(valuation error: {e})"),
        };
        writeln!(f, "    {:<14} {} shares, worth {value} in assets", h.user, h.shares)?;
    }
    match &r.unattributed {
        Ok(rest) => writeln!(
            f,
            "  unattributed     {} (rounding dust, or assets with no shares)",
            amt(*rest, d)
        ),
        Err(e) => writeln!(f, "  unattributed     (valuation error: {e})"),
    }
}

fn write_plan(f: &mut Formatter<'_>, plan: &Plan, d: Option<u8>) -> fmt::Result {
    writeln!(f, "  plan:")?;

    writeln!(f, "    targets:")?;
    if plan.targets().is_empty() {
        writeln!(f, "      (none)")?;
    }
    for t in plan.targets() {
        let bps = t.bps().map_or("not in policy".to_string(), |b| b.to_string());
        let cap = t.cap().map_or("none".to_string(), |c| amt(c, d));
        writeln!(
            f,
            "      {:<14} {bps:<14} requested {}, cap {cap}, reachable {}",
            t.strategy(),
            amt(t.requested(), d),
            amt(t.reachable(), d)
        )?;
    }

    writeln!(f, "    moves (execute in order):")?;
    if plan.moves().is_empty() {
        writeln!(f, "      (none, already on target)")?;
    }
    for (i, mv) in plan.moves().iter().enumerate() {
        match mv {
            Move::Deallocate { strategy, amount, reason } => writeln!(
                f,
                "      {}. {strategy} -> idle   {}  ({reason})",
                i + 1,
                amt(*amount, d)
            )?,
            Move::Allocate { strategy, amount } => {
                writeln!(f, "      {}. idle -> {strategy}   {}", i + 1, amt(*amount, d))?
            }
        }
    }

    writeln!(f, "    after the plan (simulated):")?;
    write_balances(f, plan.projected(), d, View::Projected)
}

/// Whole tokens when decimals are known, raw units otherwise.
fn amt(a: Assets, decimals: Option<u8>) -> String {
    match decimals {
        Some(d) => format_units(a.get(), d),
        None => format!("{} raw", a.get()),
    }
}

fn pct(part: Assets, total: Assets) -> String {
    match mul_div_floor(part.get(), 10_000, total.get()) {
        Ok(bp) => format!("{}.{:02}%", bp / 100, bp % 100),
        Err(_) => "-".into(),
    }
}
