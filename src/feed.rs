//! Turns one line of `events.jsonl` into a delivery. The only code that
//! knows the JSON format. The envelope (`block`, `log_index`, `kind`,
//! `vault`) is read before `data`, so a line with bad data can still be
//! attributed to its vault.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::event::{Delivery, Event, EventKind, ParseError, Scope};
use crate::types::{Assets, BlockNumber, EventId, Shares, StrategyId, UserId, VaultId};

const SUPPORTED_KINDS: [&str; 7] =
    ["VaultCreated", "Deposit", "Withdraw", "Accrue", "SetCap", "Allocate", "Deallocate"];

/// Parse one raw line. Bytes that aren't UTF-8 make the whole line unreadable:
/// decoding them loosely could silently change a name inside a string.
pub fn parse_bytes(line: usize, bytes: &[u8]) -> Result<Delivery, ParseError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => parse_line(line, text),
        Err(e) => Err(ParseError {
            line,
            // The readable rest may still name its vault, which is then held.
            scope: salvage_vault(&String::from_utf8_lossy(bytes)),
            reason: format!("not valid UTF-8: {e}"),
        }),
    }
}

/// Parse one line; `line` is its position in the feed.
pub fn parse_line(line: usize, text: &str) -> Result<Delivery, ParseError> {
    let wire: WireLine = serde_json::from_str(text).map_err(|e| ParseError {
        line,
        scope: salvage_vault(text),
        reason: e.to_string(),
    })?;
    let raw = match wire {
        WireLine::Reorg { from_block } => return Ok(Delivery::Reorg { from_block }),
        WireLine::Event(raw) => raw,
    };

    let id = EventId { block: raw.block, log_index: raw.log_index };
    // Missing `data` still leaves the event's vault and key known.
    let data = raw.data.as_deref().map_or("", RawValue::get);
    let kind = match raw.kind.as_str() {
        "VaultCreated" => parse_data::<VaultCreatedData>(data)
            .map(|d| EventKind::VaultCreated { decimals: d.decimals }),
        "Deposit" => parse_data::<FlowData>(data).map(|d| EventKind::Deposit {
            user: d.user,
            assets: d.assets,
            shares: d.shares,
        }),
        "Withdraw" => parse_data::<FlowData>(data).map(|d| EventKind::Withdraw {
            user: d.user,
            assets: d.assets,
            shares: d.shares,
        }),
        "Accrue" => parse_data::<AccrueData>(data).map(|d| EventKind::Accrue { assets: d.assets }),
        "SetCap" => parse_data::<CapData>(data)
            .map(|d| EventKind::SetCap { strategy: d.strategy, cap: d.cap }),
        "Allocate" => parse_data::<MoveData>(data)
            .map(|d| EventKind::Allocate { strategy: d.strategy, assets: d.assets }),
        "Deallocate" => parse_data::<MoveData>(data)
            .map(|d| EventKind::Deallocate { strategy: d.strategy, assets: d.assets }),
        _ => return Ok(Delivery::Unsupported { id, vault: raw.vault, kind: raw.kind }),
    };

    match kind {
        Ok(kind) => Ok(Delivery::Event(Event { id, vault: raw.vault, kind })),
        Err(e) => {
            let reason = match raw.data {
                None => format!("no data for {}", raw.kind),
                Some(_) => match fractional_field(data) {
                    Some(field) => {
                        format!("bad data for {}: {field} must be a whole number", raw.kind)
                    }
                    None => format!("bad data for {}: {e}", raw.kind),
                },
            };
            Err(ParseError { line, scope: Scope::Event(raw.vault, id), reason })
        }
    }
}

/// For a broken envelope: does the line still name its vault and a
/// supported kind?
fn salvage_vault(text: &str) -> Scope {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else { return Scope::Unknown };
    let field = |name| value.get("event").and_then(|e| e.get(name)).and_then(|v| v.as_str());
    match (field("vault"), field("kind")) {
        (Some(vault), Some(kind)) if SUPPORTED_KINDS.contains(&kind) => Scope::Vault(vault.into()),
        _ => Scope::Unknown,
    }
}

/// The first field holding a number with a fraction or exponent. Amounts are
/// whole numbers of the smallest unit, and serde's own error for one ("expected
/// `,` or `}`") doesn't say so. Read from the raw text, so a large integer
/// is never mistaken for a fraction.
fn fractional_field(data: &str) -> Option<String> {
    let fields: std::collections::BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(data).ok()?;
    fields.into_iter().find_map(|(name, value)| {
        let text = value.get();
        let number = text.starts_with(|c: char| c == '-' || c.is_ascii_digit());
        (number && text.contains(['.', 'e', 'E'])).then_some(name)
    })
}

fn parse_data<T: DeserializeOwned>(data: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(data)
}

// ---- wire format -----------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireLine {
    Event(RawEvent),
    Reorg { from_block: BlockNumber },
}

#[derive(Deserialize)]
struct RawEvent {
    block: BlockNumber,
    log_index: u64,
    kind: String,
    vault: VaultId,
    /// Optional, so missing data is reported against the event's vault.
    data: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct VaultCreatedData {
    decimals: u8,
}

#[derive(Deserialize)]
struct FlowData {
    user: UserId,
    assets: Assets,
    shares: Shares,
}

#[derive(Deserialize)]
struct AccrueData {
    assets: Assets,
}

#[derive(Deserialize)]
struct CapData {
    strategy: StrategyId,
    cap: Assets,
}

#[derive(Deserialize)]
struct MoveData {
    strategy: StrategyId,
    assets: Assets,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Delivery {
        parse_line(1, text).expect("should parse")
    }

    fn err(text: &str) -> ParseError {
        parse_line(7, text).expect_err("should fail")
    }

    #[test]
    fn parses_every_event_kind() {
        let deposit =
            ok(r#"{"event": {"block": 5, "log_index": 2, "kind": "Deposit", "vault": "v",
            "data": {"user": "bob", "assets": 50, "shares": 49}}}"#);
        assert_eq!(
            deposit,
            Delivery::Event(Event {
                id: EventId { block: 5, log_index: 2 },
                vault: "v".into(),
                kind: EventKind::Deposit {
                    user: "bob".into(),
                    assets: Assets::new(50),
                    shares: Shares::new(49)
                },
            })
        );

        let kinds = [
            (r#""VaultCreated", "data": {"decimals": 6}"#, EventKind::VaultCreated { decimals: 6 }),
            (
                r#""Withdraw", "data": {"user": "u", "assets": 1, "shares": 2}"#,
                EventKind::Withdraw {
                    user: "u".into(),
                    assets: Assets::new(1),
                    shares: Shares::new(2),
                },
            ),
            (r#""Accrue", "data": {"assets": 3}"#, EventKind::Accrue { assets: Assets::new(3) }),
            (
                r#""SetCap", "data": {"strategy": "s", "cap": 4}"#,
                EventKind::SetCap { strategy: "s".into(), cap: Assets::new(4) },
            ),
            (
                r#""Allocate", "data": {"strategy": "s", "assets": 5}"#,
                EventKind::Allocate { strategy: "s".into(), assets: Assets::new(5) },
            ),
            (
                r#""Deallocate", "data": {"strategy": "s", "assets": 6}"#,
                EventKind::Deallocate { strategy: "s".into(), assets: Assets::new(6) },
            ),
        ];
        for (fragment, expected) in kinds {
            let text = format!(
                r#"{{"event": {{"block": 1, "log_index": 0, "vault": "v", "kind": {fragment}}}}}"#
            );
            match ok(&text) {
                Delivery::Event(e) => assert_eq!(e.kind, expected),
                other => panic!("expected event, got {other:?}"),
            }
        }
    }

    #[test]
    fn parses_reorg() {
        assert_eq!(ok(r#"{"reorg": {"from_block": 16}}"#), Delivery::Reorg { from_block: 16 });
    }

    #[test]
    fn amounts_beyond_u64_parse() {
        // 18-decimal tokens exceed u64 quickly; amounts must not be truncated.
        let text = r#"{"event": {"block": 1, "log_index": 0, "kind": "Accrue", "vault": "v",
            "data": {"assets": 100000000000000000000000}}}"#;
        match ok(text) {
            Delivery::Event(e) => {
                assert_eq!(
                    e.kind,
                    EventKind::Accrue { assets: Assets::new(100_000_000_000_000_000_000_000) }
                )
            }
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn bad_data_is_scoped_to_its_vault_and_event() {
        let e = err(r#"{"event": {"block": 3, "log_index": 1, "kind": "Deposit", "vault": "v",
            "data": {"user": "bob", "assets": -5, "shares": 1}}}"#);
        assert_eq!(e.line, 7);
        assert_eq!(e.scope, Scope::Event("v".into(), EventId { block: 3, log_index: 1 }));
    }

    #[test]
    fn fractional_amounts_say_so() {
        for (field, data) in [
            ("shares", r#"{"user": "a", "assets": 100, "shares": 1.5}"#),
            ("assets", r#"{"user": "a", "assets": 1e6, "shares": 1}"#),
        ] {
            let e = err(&format!(
                r#"{{"event": {{"block": 1, "log_index": 0, "kind": "Deposit", "vault": "v", "data": {data}}}}}"#
            ));
            assert_eq!(e.reason, format!("bad data for Deposit: {field} must be a whole number"));
        }
        // A whole number too large for u128 keeps serde's own message.
        let e = err(r#"{"event": {"block": 1, "log_index": 0, "kind": "Accrue", "vault": "v",
            "data": {"assets": 999999999999999999999999999999999999999999}}}"#);
        assert!(!e.reason.contains("whole number"), "{}", e.reason);
    }

    #[test]
    fn missing_data_is_scoped_too() {
        let e = err(r#"{"event": {"block": 4, "log_index": 0, "kind": "Withdraw", "vault": "v"}}"#);
        assert_eq!(e.scope, Scope::Event("v".into(), EventId { block: 4, log_index: 0 }));
        assert_eq!(e.reason, "no data for Withdraw");
        let null = err(
            r#"{"event": {"block": 4, "log_index": 0, "kind": "Withdraw", "vault": "v", "data": null}}"#,
        );
        assert!(matches!(null.scope, Scope::Event(..)));
    }

    #[test]
    fn unreadable_lines_are_unscoped() {
        let cases = [
            "not json",
            r#"{}"#,
            r#"{"event": {"block": 1}, "reorg": {"from_block": 1}}"#,
            r#"{"reorg": {"from_block": "x"}}"#,
            r#"{"event": {"block": 1, "log_index": 0, "kind": "Deposit", "data": {}}}"#,
        ];
        for text in cases {
            assert_eq!(err(text).scope, Scope::Unknown, "line {text:?} should be unscoped");
        }
    }

    #[test]
    fn a_line_cut_off_mid_json_is_unscoped() {
        // The vault and kind are visible, but the line isn't valid JSON, so
        // nothing is read from it: guessing could pick text inside `data`.
        let e = err(
            r#"{"event": {"block": 3, "log_index": 0, "kind": "Withdraw", "vault": "v", "data":"#,
        );
        assert_eq!(e.scope, Scope::Unknown);
    }

    #[test]
    fn a_broken_envelope_that_names_its_vault_keeps_the_vault() {
        let bad_block = r#"{"event": {"block": "bad", "log_index": 0, "kind": "Withdraw", "vault": "v", "data": {}}}"#;
        assert_eq!(err(bad_block).scope, Scope::Vault("v".into()));
        // An unknown kind with a broken envelope stays unscoped.
        let unknown =
            r#"{"event": {"block": "bad", "log_index": 0, "kind": "Loss", "vault": "v"}}"#;
        assert_eq!(err(unknown).scope, Scope::Unknown);
    }

    #[test]
    fn invalid_utf8_is_unreadable_and_never_changes_a_name() {
        // An invalid byte inside the user's name: the line must not be
        // accepted with a changed name, and the vault it names is held.
        let line = b"{\"event\": {\"block\": 2, \"log_index\": 0, \"kind\": \"Deposit\", \"vault\": \"v\", \"data\": {\"user\": \"ali\xffce\", \"assets\": 1, \"shares\": 1}}}";
        let e = parse_bytes(4, line).unwrap_err();
        assert_eq!(e.line, 4);
        assert_eq!(e.scope, Scope::Vault("v".into()));
        assert!(e.reason.starts_with("not valid UTF-8"), "{}", e.reason);
        let e = parse_bytes(1, b"\xff\xfe").unwrap_err();
        assert_eq!(e.scope, Scope::Unknown);
    }

    #[test]
    fn unknown_kind_is_a_delivery_not_an_error() {
        let text = r#"{"event": {"block": 9, "log_index": 2, "kind": "Rebase", "vault": "v", "data": {}}}"#;
        assert_eq!(
            ok(text),
            Delivery::Unsupported {
                id: EventId { block: 9, log_index: 2 },
                vault: "v".into(),
                kind: "Rebase".into()
            }
        );
    }
}
