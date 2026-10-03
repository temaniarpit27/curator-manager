//! Reading and checking the policy file. Any problem rejects the whole file,
//! and every problem is listed. A vault the policy doesn't list gets no plan;
//! a strategy an entry doesn't list targets zero.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;
use thiserror::Error;

use crate::types::{Bps, BpsOutOfRange, StrategyId, VaultId};

/// One vault's targets, summing to at most 10 000 bps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultPolicy(BTreeMap<StrategyId, Bps>);

impl TryFrom<BTreeMap<StrategyId, Bps>> for VaultPolicy {
    type Error = OverAllocated;

    fn try_from(targets: BTreeMap<StrategyId, Bps>) -> Result<Self, OverAllocated> {
        let sum = targets.values().try_fold(0u64, |sum, b| sum.checked_add(u64::from(b.raw())));
        match sum {
            Some(sum) if sum <= u64::from(Bps::MAX) => Ok(Self(targets)),
            _ => Err(OverAllocated(sum)),
        }
    }
}

impl VaultPolicy {
    pub fn get(&self, strategy: &StrategyId) -> Option<Bps> {
        self.0.get(strategy).copied()
    }

    pub fn strategies(&self) -> impl Iterator<Item = &StrategyId> {
        self.0.keys()
    }

    /// The sum of the targets; at most 10 000, checked when built.
    pub fn total_bps(&self) -> u64 {
        self.0.values().map(|b| u64::from(b.raw())).sum()
    }
}

/// Targets summing to more than 10 000 bps.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("targets sum to {} bps, more than 10000", show_sum(.0))]
pub struct OverAllocated(pub Option<u64>);

fn show_sum(sum: &Option<u64>) -> String {
    sum.map_or_else(|| "more than u64".to_string(), |s| s.to_string())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    vaults: BTreeMap<VaultId, VaultPolicy>,
}

impl Policy {
    pub fn vault(&self, vault: &VaultId) -> Option<&VaultPolicy> {
        self.vaults.get(vault)
    }

    pub fn vaults(&self) -> impl Iterator<Item = &VaultId> {
        self.vaults.keys()
    }
}

impl FromStr for Policy {
    type Err = PolicyError;

    fn from_str(text: &str) -> Result<Self, PolicyError> {
        // Raw JSON, so a duplicate strategy key isn't silently dropped.
        let Entries(raw) = serde_json::from_str::<Entries<Box<RawValue>>>(text)
            .map_err(|e| PolicyError(vec![PolicyProblem::NotAnObject(e.to_string())]))?;

        let mut problems: Vec<PolicyProblem> =
            duplicates(&raw).into_iter().map(|v| PolicyProblem::DuplicateVault(v.into())).collect();
        let mut vaults = BTreeMap::new();
        for (vault, entry) in raw {
            let vault = VaultId::from(vault);
            match parse_vault(&vault, entry.get()) {
                Ok(targets) => {
                    vaults.insert(vault, targets);
                }
                Err(found) => problems.extend(found),
            }
        }
        if problems.is_empty() { Ok(Self { vaults }) } else { Err(PolicyError(problems)) }
    }
}

/// Every problem that rejected the policy file.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("policy rejected: {}", join(.0))]
pub struct PolicyError(pub Vec<PolicyProblem>);

fn join(problems: &[PolicyProblem]) -> String {
    problems.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

/// One problem in the policy file. `String` fields quote the file or the
/// JSON parser.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyProblem {
    #[error("not a {{vault: {{strategy: bps}}}} object: {0}")]
    NotAnObject(String),
    #[error("vault {0} is listed more than once")]
    DuplicateVault(VaultId),
    #[error("{vault}: {entry} is not a {{strategy: bps}} object")]
    VaultNotAnObject { vault: VaultId, entry: String },
    #[error("{vault}: strategy {strategy} is listed more than once")]
    DuplicateStrategy { vault: VaultId, strategy: StrategyId },
    #[error("{vault}: {strategy}: {value} is not a whole number of bps")]
    NotWholeBps { vault: VaultId, strategy: StrategyId, value: String },
    #[error("{vault}: {strategy}: {error}")]
    OutOfRange { vault: VaultId, strategy: StrategyId, error: BpsOutOfRange },
    #[error("{vault}: {error}")]
    OverAllocated { vault: VaultId, error: OverAllocated },
}

/// Parse one vault's targets, collecting every problem. The sum is checked
/// only once every value is valid.
fn parse_vault(vault: &VaultId, entry: &str) -> Result<VaultPolicy, Vec<PolicyProblem>> {
    let Entries(raw) = serde_json::from_str::<Entries<Value>>(entry).map_err(|_| {
        vec![PolicyProblem::VaultNotAnObject { vault: vault.clone(), entry: entry.to_owned() }]
    })?;
    let mut problems: Vec<PolicyProblem> = duplicates(&raw)
        .into_iter()
        .map(|s| PolicyProblem::DuplicateStrategy { vault: vault.clone(), strategy: s.into() })
        .collect();
    let mut targets = BTreeMap::new();
    for (strategy, value) in raw {
        let strategy = StrategyId::from(strategy);
        let bps = match value.as_u64() {
            None => Err(PolicyProblem::NotWholeBps {
                vault: vault.clone(),
                strategy: strategy.clone(),
                value: value.to_string(),
            }),
            Some(n) => Bps::try_from(n).map_err(|error| PolicyProblem::OutOfRange {
                vault: vault.clone(),
                strategy: strategy.clone(),
                error,
            }),
        };
        match bps {
            Ok(bps) => {
                targets.insert(strategy, bps);
            }
            Err(problem) => problems.push(problem),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    VaultPolicy::try_from(targets)
        .map_err(|error| vec![PolicyProblem::OverAllocated { vault: vault.clone(), error }])
}

fn duplicates<V>(entries: &[(String, V)]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut dups = Vec::new();
    for (k, _) in entries {
        if !seen.insert(k) && !dups.contains(k) {
            dups.push(k.clone());
        }
    }
    dups
}

/// A JSON object as a list of entries, so duplicate keys can be reported.
struct Entries<V>(Vec<(String, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Entries<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct EntriesVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for EntriesVisitor<V> {
            type Value = Entries<V>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    out.push(entry);
                }
                Ok(Entries(out))
            }
        }

        d.deserialize_map(EntriesVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_bad_entry_rejects_the_whole_file_and_lists_every_problem() {
        let err = Policy::from_str(
            r#"{
                "ok":        {"a": 5000, "b": 5000},
                "too-much":  {"a": 6000, "b": 5000},
                "negative":  {"a": -1},
                "fraction":  {"a": 12.5},
                "huge":      {"a": 20000},
                "not-obj":   5,
                "dup-strat": {"a": 6000, "a": 4000}
            }"#,
        )
        .unwrap_err();
        assert_eq!(err.0.len(), 6);
    }

    #[test]
    fn every_problem_within_one_vault_is_reported() {
        let err = Policy::from_str(r#"{"v": {"a": -1, "b": 12.5, "c": 20000, "d": 1, "d": 2}, "w": {"a": 50000, "b": -7}}"#)
            .unwrap_err();
        assert_eq!(err.0.len(), 6, "{:?}", err.0);
        assert!(err.0.iter().any(|e| e.to_string() == "v: strategy d is listed more than once"));
    }

    #[test]
    fn duplicate_vault_rejects_the_file() {
        let err = Policy::from_str(r#"{"v": {"a": 6000}, "v": {"a": 4000}}"#).unwrap_err();
        assert_eq!(err.0, [PolicyProblem::DuplicateVault("v".into())]);
    }

    #[test]
    fn vault_policy_cannot_exceed_100_percent() {
        let full = Bps::try_from(10_000).unwrap();
        let targets = BTreeMap::from([("a".into(), full), ("b".into(), full)]);
        assert!(VaultPolicy::try_from(targets).is_err());
    }

    #[test]
    fn wrong_shape_is_rejected() {
        assert!(Policy::from_str("[1, 2]").is_err());
        assert!(Policy::from_str("5").is_err());
        assert!(Policy::from_str("not json").is_err());
    }

    #[test]
    fn empty_policy_is_valid() {
        assert_eq!(Policy::from_str("{}").unwrap(), Policy::default());
    }
}
