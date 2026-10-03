//! Reads the feed into the engine ([`ingest`]), then decides for each vault
//! whether it can be planned and builds the report ([`report`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead};

use crate::engine::Engine;
use crate::feed::parse_bytes;
use crate::planner::plan;
use crate::policy::{Policy, PolicyError};
use crate::report::{Holding, Outcome, Report, VaultReport, WithheldReason};
use crate::types::{Assets, Bps, MathError, VaultId, format_units};
use crate::vault::Vault;

/// Feed every line to a new engine. Only a failure to read is an error.
pub fn ingest<R: BufRead>(reader: R) -> io::Result<Engine> {
    let mut engine = Engine::default();
    for (index, bytes) in reader.split(b'\n').enumerate() {
        let bytes = bytes?;
        let line = index + 1;
        if bytes.trim_ascii().is_empty() {
            continue;
        }
        match parse_bytes(line, &bytes) {
            Ok(delivery) => engine.ingest(line, delivery),
            Err(error) => engine.reject(error),
        };
    }
    engine.finish();
    Ok(engine)
}

/// Report every vault, and plan every vault that can be.
pub fn report(engine: &Engine, policy: &Result<Policy, PolicyError>) -> Report {
    let policy_errors = policy.as_ref().err().map(|e| e.0.clone()).unwrap_or_default();
    let policy = policy.as_ref().ok();

    // Include vaults known only from a malformed line, so they are reported.
    let mut names: BTreeSet<&VaultId> = engine.vaults().keys().collect();
    if let Some(p) = policy {
        names.extend(p.vaults());
    }
    names.extend(engine.vaults_with_input_issues());

    let vaults: BTreeMap<VaultId, VaultReport> = names
        .into_iter()
        .map(|name| {
            let report = vault_report(name, engine, policy);
            if let Outcome::Withheld(reasons) = &report.outcome {
                for reason in reasons {
                    tracing::error!(vault = %name, %reason, "no plan for vault");
                }
            }
            (name.clone(), report)
        })
        .collect();

    tracing::info!(stats = ?engine.stats(), "run finished");

    Report { warnings: engine.warnings().to_vec(), policy_errors, vaults }
}

fn vault_report(name: &VaultId, engine: &Engine, policy: Option<&Policy>) -> VaultReport {
    let projection = engine.vaults().get(name);
    let state = projection.map(|p| p.state().clone()).unwrap_or_default();
    let (holdings, unattributed) = valuations(&state);
    let notes = notes(&state);
    let issues = engine.input_issues(name);

    // Collect every reason not to plan; plan only if there are none.
    let targets = policy.and_then(|p| p.vault(name));
    if let Some(t) = targets.filter(|t| t.total_bps() < u64::from(Bps::MAX)) {
        tracing::info!(vault = %name, bps = t.total_bps(), "policy targets sum to less than 100%; the rest stays idle");
    }
    let mut reasons = Vec::new();
    match policy {
        None => reasons.push(WithheldReason::PolicyRejected),
        Some(_) if targets.is_none() => reasons.push(WithheldReason::NoPolicyEntry),
        Some(_) => {}
    }
    if !issues.is_empty() {
        reasons.push(WithheldReason::UntrustedInput(issues));
    }
    match projection.map(|p| p.unresolved()) {
        None => reasons.push(WithheldReason::NeverCreated),
        Some(Some(u)) => {
            reasons.push(WithheldReason::Unresolved { event: u.event, error: u.error.clone() })
        }
        Some(None) => {}
    }
    let conflicting = &state.observations.conflicting_creations;
    if !conflicting.is_empty() {
        reasons.push(WithheldReason::ConflictingCreation(conflicting.clone()));
    }
    let unbacked = &state.observations.unbacked_flows;
    if !unbacked.is_empty() {
        reasons.push(WithheldReason::UnbackedFlow(unbacked.clone()));
    }
    let outcome = match targets {
        Some(targets) if reasons.is_empty() => match plan(&state, targets) {
            Ok(plan) => Outcome::Planned(Box::new(plan)),
            Err(e) => Outcome::Withheld(vec![WithheldReason::PlannerRefused(e)]),
        },
        _ => Outcome::Withheld(reasons),
    };

    VaultReport { state, holdings, unattributed, notes, outcome }
}

/// Each holder's value, and the total minus their sum (rounding dust).
fn valuations(state: &Vault) -> (Vec<Holding>, Result<Assets, MathError>) {
    let mut holdings = Vec::with_capacity(state.holders.len());
    let mut attributed = Ok(Assets::ZERO);
    for (user, shares) in &state.holders {
        let value = state.value_of(*shares);
        attributed = match (attributed, &value) {
            (Ok(sum), Ok(Some(v))) => sum.checked_add(*v),
            (Ok(sum), Ok(None)) => Ok(sum),
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e.clone()),
        };
        holdings.push(Holding { user: user.clone(), shares: *shares, value });
    }
    let unattributed = attributed.and_then(|sum| state.total_assets()?.checked_sub(sum));
    (holdings, unattributed)
}

fn notes(state: &Vault) -> Vec<String> {
    let mut notes = Vec::new();
    for id in &state.observations.repeated_creations {
        notes.push(format!(
            "VaultCreated repeated at {id}: ignored; the first in chain order stands"
        ));
    }
    for id in &state.observations.rounded_flows {
        notes.push(format!(
            "deposit or withdrawal at {id} rounded one side to zero (no shares minted, or no assets paid); applied as reported"
        ));
    }
    for (strategy, id) in &state.observations.cap_breaches {
        notes.push(format!(
            "{strategy}: allocation at {id} took the balance above the cap in force at that point in the received history; the cause can't be determined from the feed"
        ));
    }
    let amount = |a: Assets| match state.decimals {
        Some(d) => format_units(a.get(), d),
        None => format!("{} raw", a.get()),
    };
    for (strategy, s) in &state.strategies {
        if s.is_over_cap() {
            notes.push(format!(
                "{strategy}: holds {} above its effective cap {}; a corrective move is planned if the vault is plannable",
                amount(s.balance),
                amount(s.effective_cap())
            ));
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::Plan;

    const FEED: &str = include_str!("../data/events.jsonl");

    fn planned<'a>(report: &'a Report, vault: &str) -> &'a Plan {
        match &report.vaults[vault].outcome {
            Outcome::Planned(p) => p,
            Outcome::Withheld(r) => panic!("{vault} should be planned, withheld because {r:?}"),
        }
    }

    fn withheld<'a>(report: &'a Report, vault: &str) -> &'a [WithheldReason] {
        match &report.vaults[vault].outcome {
            Outcome::Withheld(r) => r,
            Outcome::Planned(_) => panic!("{vault} should not be planned"),
        }
    }

    /// Ingest a whole string, then report.
    fn run(feed: &str, policy_text: &str) -> Report {
        let engine = ingest(feed.as_bytes()).expect("a byte slice reads");
        report(&engine, &policy_text.parse())
    }

    #[test]
    fn policy_only_vault_is_unresolved() {
        let report =
            run(FEED, r#"{"vault-core": {}, "vault-yield": {}, "vault-ghost": {"aave": 100}}"#);
        assert_eq!(withheld(&report, "vault-ghost"), [WithheldReason::NeverCreated]);
        planned(&report, "vault-core");
    }

    #[test]
    fn zero_shares_leave_valuation_undefined_but_planning_continues() {
        let feed = r#"{"event": {"block": 1, "log_index": 0, "kind": "VaultCreated", "vault": "v", "data": {"decimals": 6}}}
{"event": {"block": 2, "log_index": 0, "kind": "Accrue", "vault": "v", "data": {"assets": 10}}}"#;
        let report = run(feed, r#"{"v": {}}"#);
        assert_eq!(report.vaults["v"].unattributed, Ok(Assets::new(10)));
        planned(&report, "v");
    }

    const CREATE_V: &str = r#"{"event": {"block": 1, "log_index": 0, "kind": "VaultCreated", "vault": "v", "data": {"decimals": 0}}}"#;
    const DEPOSIT_V: &str = r#"{"event": {"block": 2, "log_index": 0, "kind": "Deposit", "vault": "v", "data": {"user": "a", "assets": 100, "shares": 100}}}"#;

    #[test]
    fn a_malformed_line_that_names_its_vault_holds_it() {
        // Bad data, missing data, and a broken envelope that still names the
        // vault and a supported kind.
        let bad_data = r#"{"event": {"block": 3, "log_index": 0, "kind": "Deposit", "vault": "v", "data": {"user": "a", "assets": "x", "shares": 1}}}"#;
        let no_data =
            r#"{"event": {"block": 3, "log_index": 0, "kind": "Withdraw", "vault": "v"}}"#;
        let bad_block = r#"{"event": {"block": "bad", "log_index": 0, "kind": "Withdraw", "vault": "v", "data": {"user": "a", "assets": 80, "shares": 80}}}"#;
        for bad in [bad_data, no_data, bad_block] {
            let report = run(&[CREATE_V, DEPOSIT_V, bad].join("\n"), r#"{"v": {}}"#);
            let reasons = withheld(&report, "v");
            assert!(matches!(reasons[..], [WithheldReason::UntrustedInput(_)]), "{bad}");
        }

        // A vault named only by such a line is still reported, with every
        // reason; the healthy vault is still planned.
        let bad_w = bad_data.replace(r#""vault": "v""#, r#""vault": "w""#);
        let report = run(&[CREATE_V, DEPOSIT_V, &bad_w].join("\n"), r#"{"v": {}}"#);
        let w = withheld(&report, "w");
        assert!(w.iter().any(|r| matches!(r, WithheldReason::UntrustedInput(_))), "{w:?}");
        assert!(w.contains(&WithheldReason::NeverCreated), "every reason stays visible");
        planned(&report, "v");
    }

    #[test]
    fn a_creation_with_different_decimals_holds_the_vault() {
        let again = |decimals: u8| {
            format!(
                r#"{{"event": {{"block": 5, "log_index": 0, "kind": "VaultCreated", "vault": "v", "data": {{"decimals": {decimals}}}}}}}"#
            )
        };
        let report = run(&[CREATE_V, DEPOSIT_V, &again(18)].join("\n"), r#"{"v": {}}"#);
        let reasons = withheld(&report, "v");
        assert!(matches!(reasons[..], [WithheldReason::ConflictingCreation(_)]), "{reasons:?}");
        // A repeat with the same decimals is harmless.
        let report = run(&[CREATE_V, DEPOSIT_V, &again(0)].join("\n"), r#"{"v": {}}"#);
        planned(&report, "v");
        // A reorg that removes the conflicting creation removes the hold.
        let reorg = r#"{"reorg": {"from_block": 5}}"#;
        let report = run(&[CREATE_V, DEPOSIT_V, &again(18), reorg].join("\n"), r#"{"v": {}}"#);
        planned(&report, "v");
    }

    #[test]
    fn blocked_accounting_and_untrusted_input_are_both_shown() {
        let overdraw = r#"{"event": {"block": 3, "log_index": 0, "kind": "Withdraw", "vault": "v", "data": {"user": "a", "assets": 500, "shares": 1}}}"#;
        let clash = r#"{"event": {"block": 2, "log_index": 0, "kind": "Deposit", "vault": "v", "data": {"user": "a", "assets": 7, "shares": 7}}}"#;
        let report = run(&[CREATE_V, DEPOSIT_V, overdraw, clash].join("\n"), r#"{"v": {}}"#);
        let reasons = withheld(&report, "v");
        assert!(matches!(reasons[0], WithheldReason::UntrustedInput(_)), "{reasons:?}");
        assert!(matches!(reasons[1], WithheldReason::Unresolved { .. }), "{reasons:?}");
    }

    #[test]
    fn maximum_amounts_are_valued_and_planned_exactly() {
        let max = u128::MAX;
        let lines = [
            r#"{"event": {"block": 1, "log_index": 0, "kind": "VaultCreated", "vault": "v", "data": {"decimals": 0}}}"#.to_string(),
            format!(r#"{{"event": {{"block": 2, "log_index": 0, "kind": "Deposit", "vault": "v", "data": {{"user": "a", "assets": {max}, "shares": {max}}}}}}}"#),
            format!(r#"{{"event": {{"block": 3, "log_index": 0, "kind": "SetCap", "vault": "v", "data": {{"strategy": "s", "cap": {max}}}}}}}"#),
        ];
        let feed = lines.join("\n");
        let report = run(&feed, r#"{"v": {"s": 10000}}"#);
        let v = &report.vaults["v"];
        assert_eq!(v.holdings[0].value, Ok(Some(Assets::new(max))));
        assert_eq!(v.unattributed, Ok(Assets::ZERO));
        let plan = planned(&report, "v");
        assert_eq!(plan.moves().len(), 1);
        assert_eq!(plan.moves()[0].amount(), Assets::new(max));
        assert_eq!(plan.projected().idle, Assets::ZERO);
    }

    #[test]
    fn overflow_halts_only_its_vault() {
        let max = u128::MAX;
        let mut feed = String::new();
        for vault in ["big", "ok"] {
            feed += &format!(
                r#"{{"event": {{"block": 1, "log_index": {}, "kind": "VaultCreated", "vault": "{vault}", "data": {{"decimals": 0}}}}}}"#,
                u8::from(vault == "ok")
            );
            feed += "\n";
        }
        feed += &format!(
            r#"{{"event": {{"block": 2, "log_index": 0, "kind": "Deposit", "vault": "big", "data": {{"user": "a", "assets": {max}, "shares": 1}}}}}}"#
        );
        feed += "\n";
        feed += r#"{"event": {"block": 3, "log_index": 0, "kind": "Accrue", "vault": "big", "data": {"assets": 1}}}"#;
        feed += "\n";
        feed += r#"{"event": {"block": 3, "log_index": 1, "kind": "Deposit", "vault": "ok", "data": {"user": "b", "assets": 5, "shares": 5}}}"#;
        let report = run(&feed, r#"{"big": {}, "ok": {}}"#);
        assert!(matches!(report.vaults["big"].outcome, Outcome::Withheld(_)));
        assert_eq!(report.vaults["big"].state.idle, Assets::new(max), "the deposit stands");
        assert!(matches!(report.vaults["ok"].outcome, Outcome::Planned(_)));
    }
}
