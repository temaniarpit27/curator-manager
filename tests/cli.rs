//! End-to-end tests: run the built program on the files in `data/` and on
//! one-vault scenarios in `tests/data/`, read the printed report, and check
//! every plan independently of the planner.

// Test code: a panic here is a test failure, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::process::{Command, Output};

use serde_json::Value;

/// The vault every scenario in `tests/data/` uses.
const VAULT: &str = "vault-core";
/// One whole token, with 6 decimals.
const T: u128 = 1_000_000;

/// One vault as printed in the report. Amounts are raw units.
#[derive(Debug, Default)]
struct VaultOut {
    planned: bool,
    /// Lines starting with "!" under the vault's heading.
    reasons: Vec<String>,
    total: u128,
    idle: u128,
    shares: u128,
    /// Strategy -> (balance, cap). `None` means no cap is set.
    strategies: BTreeMap<String, (u128, Option<u128>)>,
    /// User -> (shares, value).
    holders: BTreeMap<String, (u128, u128)>,
    /// (from, to, amount); "idle" is one end of every move.
    moves: Vec<(String, String, u128)>,
    /// The printed state after the plan, including "idle".
    after: BTreeMap<String, u128>,
}

impl VaultOut {
    fn moves(&self) -> Vec<(&str, &str, u128)> {
        self.moves.iter().map(|(f, t, a)| (f.as_str(), t.as_str(), *a)).collect()
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_curator-manager"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("the binary runs")
}

/// Run `tests/data/<name>`; return the whole report, the vault, and the policy.
fn scenario(name: &str) -> (String, VaultOut, Value) {
    let events = format!("tests/data/{name}/events.jsonl");
    let policy = format!("tests/data/{name}/policy.json");
    let out = run(&["--events", &events, "--policy", &policy]);
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let mut vaults = parse(&text);
    assert_eq!(vaults.len(), 1, "{name}: one vault per scenario");
    let vault = vaults.remove(VAULT).expect("the scenario's vault is reported");
    let policy = std::fs::read_to_string(format!("{}/{policy}", env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .parse()
        .unwrap();
    (text, vault, policy)
}

/// "525000.000000" -> 525000000000. All the data uses 6 decimals.
fn raw(amount: &str) -> u128 {
    let (whole, frac) = amount.split_once('.').expect("6 decimals");
    assert_eq!(frac.len(), 6, "{amount}");
    format!("{whole}{frac}").parse().unwrap()
}

fn parse(report: &str) -> BTreeMap<String, VaultOut> {
    let mut vaults = BTreeMap::new();
    let mut current: Option<(String, VaultOut)> = None;
    let mut section = "";
    for line in report.lines() {
        if let Some(rest) = line.strip_prefix("== Vault ") {
            if let Some((name, v)) = current.take() {
                vaults.insert(name, v);
            }
            let (name, status) = rest.split_once(' ').unwrap();
            let v = VaultOut { planned: status == "[planned]", ..VaultOut::default() };
            current = Some((name.to_string(), v));
            section = "heading";
            continue;
        }
        let Some((_, v)) = current.as_mut() else { continue };
        match line.trim_end() {
            "  current state:" => section = "state",
            l if l.starts_with("  state so far (stopped at event ") => section = "state",
            "  holders:" => section = "holders",
            "  plan:" => section = "plan",
            "    targets:" => section = "targets",
            "    moves (execute in order):" => section = "moves",
            "    after the plan (simulated):" => section = "after",
            l if l.starts_with("  ! ") => v.reasons.push(l[4..].to_string()),
            l if l.starts_with("  unattributed") || l.trim().is_empty() => {}
            l => {
                let words: Vec<&str> = l.split_whitespace().collect();
                match section {
                    "state" => match words[..] {
                        ["total", "assets", x] => v.total = raw(x),
                        ["idle", x, _] => v.idle = raw(x),
                        ["shares", "(raw)", n] => v.shares = n.parse().unwrap(),
                        [name, x, _, "cap", c, ..] => {
                            let c = c.trim_end_matches(',');
                            let cap = (c != "none").then(|| raw(c));
                            v.strategies.insert(name.to_string(), (raw(x), cap));
                        }
                        _ => panic!("state line: {l}"),
                    },
                    "holders" => match words[..] {
                        ["(none)"] => {}
                        [user, shares, "shares,", "worth", x, "in", "assets"] => {
                            v.holders.insert(user.to_string(), (shares.parse().unwrap(), raw(x)));
                        }
                        _ => panic!("holder line: {l}"),
                    },
                    "moves" => match words[..] {
                        ["(none,", "already", "on", "target)"] => {}
                        [_, from, "->", to, x, ..] => {
                            v.moves.push((from.to_string(), to.to_string(), raw(x)))
                        }
                        _ => panic!("move line: {l}"),
                    },
                    "after" => match words[..] {
                        ["total", "assets", x] => assert_eq!(raw(x), v.total, "total changed"),
                        [name, x, _] => {
                            v.after.insert(name.to_string(), raw(x));
                        }
                        _ => panic!("after line: {l}"),
                    },
                    _ => {}
                }
            }
        }
    }
    if let Some((name, v)) = current {
        vaults.insert(name, v);
    }
    vaults
}

/// Check a planned vault against the brief, using only the printed report and
/// the policy file:
/// - the printed state adds up, and each holder's value follows from shares;
/// - each move takes only what its source holds at that point;
/// - moves out of strategies come first, and each strategy moves at most once;
/// - assets are conserved and no strategy ends above its cap;
/// - every strategy ends at min(floor(total * bps / 10000), cap), with a
///   missing cap counted as zero and a strategy missing from the policy at zero;
/// - a policy strategy the vault has never seen gets no move;
/// - the result matches the printed "after the plan" state.
fn check_plan(name: &str, v: &VaultOut, policy: &Value) {
    assert!(v.planned, "{name} should be planned, but: {:?}", v.reasons);
    let in_strategies: u128 = v.strategies.values().map(|(b, _)| b).sum();
    assert_eq!(v.idle + in_strategies, v.total, "{name}: state doesn't add up");
    let held: u128 = v.holders.values().map(|(s, _)| s).sum();
    assert_eq!(held, v.shares, "{name}: holder shares don't add up");
    for (user, (shares, value)) in &v.holders {
        assert_eq!(*value, shares * v.total / v.shares, "{name}: {user}'s value");
    }

    let mut balances: BTreeMap<String, u128> =
        v.strategies.iter().map(|(s, (b, _))| (s.clone(), *b)).collect();
    balances.insert("idle".into(), v.idle);
    let mut moved = Vec::new();
    let mut allocating = false;
    for (from, to, amount) in &v.moves {
        let strategy = if from == "idle" { to } else { from };
        assert!(!moved.contains(strategy), "{name}: {strategy} moves twice");
        moved.push(strategy.clone());
        if from == "idle" {
            allocating = true;
        } else {
            assert!(!allocating, "{name}: a move out of {from} comes after a move in");
        }
        let source = balances.get_mut(from).expect("source exists");
        assert!(*source >= *amount, "{name}: {from} holds {source}, the move takes {amount}");
        *source -= amount;
        *balances.get_mut(to).expect("destination exists") += amount;
    }

    let total_after: u128 = balances.values().sum();
    assert_eq!(total_after, v.total, "{name}: assets not conserved");
    let targets = &policy[name];
    for (strategy, (_, cap)) in &v.strategies {
        let cap = cap.unwrap_or(0);
        let end = balances[strategy];
        assert!(end <= cap, "{name}: {strategy} ends at {end}, above its cap {cap}");
        let expected = match targets.get(strategy).and_then(Value::as_u64) {
            Some(bps) => (v.total * u128::from(bps) / 10_000).min(cap),
            None => 0,
        };
        assert_eq!(end, expected, "{name}: {strategy} misses its target");
    }
    if let Some(entries) = targets.as_object() {
        for strategy in entries.keys().filter(|s| !v.strategies.contains_key(*s)) {
            assert!(!moved.contains(strategy), "{name}: {strategy} has no cap but is moved");
        }
    }
    assert_eq!(balances, v.after, "{name}: printed result differs from the moves");
}

// ---- the supplied data -------------------------------------------------------

#[test]
fn the_supplied_data() {
    let out = run(&[]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let vaults = parse(&text);
    let policy: Value = include_str!("../data/policy.json").parse().unwrap();

    let core = &vaults["vault-core"];
    check_plan("vault-core", core, &policy);
    assert_eq!((core.total, core.idle), (184_000 * T, 54_000 * T));
    assert_eq!(core.shares, 179_307_410_316);
    // Grace's 500k deposit was cut by the reorg; her 50k replacement counts.
    assert_eq!(core.holders["grace"].0, 49_260_277_559);
    assert_eq!(core.moves(), [("aave", "idle", 19_600 * T), ("idle", "spark", 30_000 * T)]);

    let yld = &vaults["vault-yield"];
    check_plan("vault-yield", yld, &policy);
    assert_eq!((yld.total, yld.idle), (403_000 * T, 83_000 * T));
    assert_eq!(yld.strategies["morpho"], (240_000 * T, Some(200_000 * T)), "over its cap");
    assert_eq!(yld.moves(), [("morpho", "idle", 78_800 * T), ("idle", "aave", 121_500 * T)]);

    let legacy = &vaults["vault-legacy"];
    assert!(!legacy.planned);
    assert!(legacy.reasons.iter().any(|r| r.starts_with("no entry in the policy")));
    assert!(legacy.reasons.iter().any(|r| r.contains("no VaultCreated earlier in chain order")));
}

// ---- clean input: the vault is planned ---------------------------------------

#[test]
fn deposits_and_an_allocation() {
    let (_, v, policy) = scenario("deposit_and_allocate");
    assert_eq!((v.total, v.idle, v.shares), (500_000 * T, 400_000 * T, 500_000 * T));
    assert_eq!(v.holders["alice"], (300_000 * T, 300_000 * T));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 200_000 * T), ("idle", "spark", 150_000 * T)]);
}

#[test]
fn yield_raises_share_value_and_a_withdrawal_burns_shares() {
    let (_, v, policy) = scenario("yield_and_withdraw");
    // 100k in, 10k yield, bob buys 50k shares for 55k, alice burns 20k
    // shares for 22k, then 10k comes back from aave.
    assert_eq!((v.total, v.idle, v.shares), (143_000 * T, 93_000 * T, 130_000 * T));
    assert_eq!(v.strategies["aave"].0, 50_000 * T);
    assert_eq!(v.holders["alice"], (80_000 * T, 88_000 * T));
    assert_eq!(v.holders["bob"], (50_000 * T, 55_000 * T));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 21_500 * T)]);
}

#[test]
fn a_vault_already_on_target_needs_no_moves() {
    let (text, v, policy) = scenario("already_on_target");
    check_plan(VAULT, &v, &policy);
    assert!(v.moves.is_empty());
    assert!(text.contains("(none, already on target)"));
}

#[test]
fn rebalancing_goes_through_idle_and_rounds_down() {
    let (_, v, policy) = scenario("rebalance_with_rounding");
    // 33.33% of 1,000,000.000340 rounds down to 333,300.000113; the rounding
    // stays idle.
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("aave", "idle", 366_699_999_887), ("idle", "morpho", 333_300_000_113)]);
    assert_eq!(v.after["idle"], 333_400_000_114);
}

#[test]
fn an_empty_policy_entry_moves_everything_to_idle() {
    let (text, v, policy) = scenario("empty_policy_moves_all_to_idle");
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("aave", "idle", 200_000 * T), ("spark", "idle", 70_000 * T)]);
    assert!(text.contains("aave -> idle   200000.000000  (not in policy)"));
    assert_eq!(v.after["idle"], v.total);
}

#[test]
fn caps_are_never_exceeded_and_blocked_money_stays_idle() {
    let (text, v, policy) = scenario("caps");
    check_plan(VAULT, &v, &policy);
    assert_eq!(
        v.moves(),
        [
            ("compound", "idle", 150_000 * T), // not in the policy
            ("morpho", "idle", 250_000 * T),   // its cap was lowered to 200k
            ("idle", "aave", 400_000 * T),
            ("idle", "spark", 50_000 * T), // 200k asked, capped at 50k
        ]
    );
    assert!(text.contains("morpho -> idle   250000.000000  (above cap)"));
    assert!(text.contains("! OVER CAP"));
    // ether-fi has no cap on chain, so it can't be funded.
    assert!(text.contains("requested 100000.000000, cap none, reachable 0.000000"));
    assert_eq!(v.after["idle"], 350_000 * T);
}

#[test]
fn late_and_duplicate_events_give_the_in_order_state() {
    let (_, v, policy) = scenario("late_and_duplicate_events");
    // Bob's late 100k counts once, alice's duplicate is ignored.
    assert_eq!((v.total, v.idle, v.shares), (505_000 * T, 255_000 * T, 500_000 * T));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 2_500 * T)]);
}

#[test]
fn a_reorg_replaces_the_removed_blocks() {
    let (_, v, policy) = scenario("reorg_with_replacement");
    // The reorg drops carol's 50k deposit and the 100k allocation to spark;
    // her 40k replacement counts instead.
    assert_eq!((v.total, v.idle, v.shares), (425_000 * T, 425_000 * T, 419_400 * T));
    assert_eq!(v.strategies["spark"].0, 0);
    assert_eq!(v.holders["carol"].0, 39_200 * T);
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 212_500 * T), ("idle", "spark", 127_500 * T)]);
}

#[test]
fn a_late_earlier_event_fixes_a_stuck_vault() {
    let (_, v, policy) = scenario("fixed_by_a_late_event");
    // The withdrawal came before bob's deposit; the late deposit fixes it.
    assert_eq!((v.total, v.shares), (80_000 * T, 80_000 * T));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 80_000 * T)]);
}

#[test]
fn rounding_a_flow_to_zero_is_a_note_not_a_hold() {
    let (text, v, policy) = scenario("rounding_to_zero");
    // A share redeemed for 0 assets and 1 raw asset deposited for 0 shares:
    // both round in the vault's favour, so the vault is still planned.
    assert!(text.contains("deposit or withdrawal at 3:0 rounded one side to zero"));
    assert!(text.contains("deposit or withdrawal at 4:0 rounded one side to zero"));
    assert_eq!((v.total, v.shares), (100_000 * T + 1, 100_000 * T - 1));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 100_000 * T + 1)]);
}

#[test]
fn unknown_kinds_and_unreadable_lines_are_only_warnings() {
    let (text, v, policy) = scenario("warnings_only");
    assert!(text.contains("unknown kind \"Rebase\"; not applied"));
    assert!(text.contains("line 5: malformed: "));
    assert!(text.contains("line 6: malformed: "), "a reorg with no from_block");
    // Cut off mid-line: it isn't valid JSON, so it counts as unreadable even
    // though the vault and kind are visible in the text.
    assert!(text.contains("line 7: malformed: "));
    check_plan(VAULT, &v, &policy);
    assert_eq!(v.moves(), [("idle", "aave", 100_000 * T)]);
}

// ---- the vault is not planned ------------------------------------------------

#[test]
fn a_stuck_vault_is_not_planned() {
    let (text, v, _) = scenario("stuck_vault");
    assert!(!v.planned);
    assert_eq!(v.reasons.len(), 1);
    assert!(v.reasons[0].starts_with("unresolved at event 3:0: idle: insufficient"));
    assert!(text.contains("state so far (stopped at event 3:0; later events not applied):"));
    // The deposit queued behind the stuck withdrawal isn't applied.
    assert_eq!(v.total, 10_000 * T);
}

#[test]
fn a_broken_event_naming_the_vault_holds_its_plan() {
    let (text, v, _) = scenario("broken_event");
    assert!(!v.planned);
    assert!(v.reasons[0].contains("ambiguous or incomplete"));
    assert!(text.contains("assets must be a whole number"));
    assert_eq!(v.total, 100_000 * T, "the state before it is still shown");
}

#[test]
fn a_withdrawal_that_burns_no_shares_holds_the_plan() {
    let (_, v, _) = scenario("one_sided_withdrawal");
    assert!(!v.planned);
    assert!(
        v.reasons[0]
            .starts_with("a deposit or withdrawal at 3:0 gave assets or shares for nothing")
    );
    // Applied as reported: 90k left without burning shares, so alice's
    // 100k shares are now worth 10k.
    assert_eq!(v.total, 10_000 * T);
    assert_eq!(v.holders["alice"], (100_000 * T, 10_000 * T));
}

#[test]
fn two_different_events_with_one_key_hold_the_plan() {
    let (text, v, _) = scenario("conflicting_event");
    assert!(!v.planned);
    assert!(text.contains("line 3: two different payloads for event 2:0"));
    assert_eq!(v.total, 50_000 * T, "the first payload is kept");
}

#[test]
fn a_line_that_is_not_valid_utf8_holds_its_vault() {
    let (text, v, _) = scenario("invalid_utf8");
    assert!(!v.planned);
    assert!(text.contains("line 3: malformed vault-core event: not valid UTF-8"));
    // Bob's deposit isn't applied under a changed name.
    assert_eq!(v.holders.keys().collect::<Vec<_>>(), ["alice"]);
    assert_eq!(v.total, 100_000 * T);
}

#[test]
fn a_vault_missing_from_the_policy_is_not_planned() {
    let out = run(&[
        "--events",
        "tests/data/missing_from_policy/events.jsonl",
        "--policy",
        "tests/data/missing_from_policy/policy.json",
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    let v = &parse(&text)[VAULT];
    assert!(!v.planned);
    assert!(v.reasons[0].starts_with("no entry in the policy"));
    assert_eq!(v.total, 100_000 * T);
}

#[test]
fn a_vault_never_created_has_no_state() {
    let (text, v, _) = scenario("never_created");
    assert!(!v.planned);
    assert!(v.reasons[0].contains("unresolved at event 2:0: no VaultCreated earlier"));
    assert!(text.contains("current state: not derived (no VaultCreated applied)"));
}

#[test]
fn a_policy_file_with_mistakes_plans_nothing() {
    let out = run(&[
        "--events",
        "tests/data/bad_policy/events.jsonl",
        "--policy",
        "tests/data/bad_policy/policy.json",
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("policy rejected (no vault is planned):"));
    assert!(text.contains("vault-core: targets sum to 11000 bps, more than 10000"));
    assert!(text.contains("vault-other: aave: 20000 bps is outside 0..=10000"));
    let v = &parse(&text)[VAULT];
    assert!(!v.planned);
    assert_eq!(v.total, 400_000 * T, "the state is still shown");
}

// ---- the command line --------------------------------------------------------

#[test]
fn help_prints_usage_and_succeeds() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("usage: curator-manager [--events <path>] [--policy <path>]"));
        assert!(!text.contains("== Vault"), "help doesn't run the program");
    }
}

#[test]
fn bad_arguments_and_missing_files_fail() {
    let out = run(&["--bogus"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument --bogus"));

    let out = run(&["--events"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--events needs a path"));

    let out = run(&["--events", "tests/data/missing.jsonl"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reading tests/data/missing.jsonl"));
}
