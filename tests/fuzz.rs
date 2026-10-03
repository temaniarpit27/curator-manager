//! No panics on broken input: run the library on 700 randomly damaged feeds
//! and policies built from the files in `data/` and `tests/data/`. Any panic
//! fails the test. A fixed seed makes every run the same.

// Test code: a panic here is a test failure, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;

use curator_manager::run::{ingest, report};
use serde_json::{Value, json};

const CASES: usize = 700;

/// A small deterministic random generator (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Every event line in the repo's data files.
fn sample_lines() -> Vec<String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut dirs: Vec<_> =
        fs::read_dir(format!("{root}/tests/data")).unwrap().map(|d| d.unwrap().path()).collect();
    // Directory order differs between filesystems; sorting keeps the seed
    // meaningful everywhere.
    dirs.sort();
    let mut files = vec![format!("{root}/data/events.jsonl")];
    files.extend(dirs.iter().map(|d| format!("{}/events.jsonl", d.display())));
    files
        .iter()
        .flat_map(|f| {
            String::from_utf8_lossy(&fs::read(f).unwrap())
                .lines()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Stands in for an odd value inside a JSON value, until it is written out.
const PLACEHOLDER: &str = "\u{1}odd";

/// Odd values as raw JSON text: huge, negative, fractional, wrong type,
/// missing. Raw text keeps u128::MAX and the number just past it exact; a
/// `serde_json::Value` would turn both into the same float.
fn odd_value(rng: &mut Rng) -> &'static str {
    let values = [
        "0",
        "-1",
        "1.5",
        "1e40",
        "18446744073709551615",
        "340282366920938463463374607431768211455",
        "340282366920938463463374607431768211456",
        "\"7\"",
        "null",
        "true",
        "[]",
        "{}",
    ];
    rng.pick(&values)
}

/// Damage one line: cut it, flip a byte, or change one field.
fn damage(rng: &mut Rng, line: &str) -> Vec<u8> {
    let mut bytes = line.as_bytes().to_vec();
    match rng.below(4) {
        0 => {
            bytes.truncate(rng.below(bytes.len() + 1));
            return bytes;
        }
        1 if !bytes.is_empty() => {
            let i = rng.below(bytes.len());
            bytes[i] = rng.next() as u8;
            return bytes;
        }
        _ => {}
    }
    let Ok(mut value) = serde_json::from_str::<Value>(line) else { return bytes };
    let odd = odd_value(rng);
    if let Some(event) = value.get_mut("event").and_then(Value::as_object_mut) {
        let field = *rng.pick(&["block", "log_index", "kind", "vault", "data"]);
        match field {
            "kind" => {
                let kinds =
                    ["Deposit", "Withdraw", "Accrue", "SetCap", "VaultCreated", "Rebase", ""];
                event.insert(field.into(), json!(rng.pick(&kinds)));
            }
            "vault" => {
                event
                    .insert(field.into(), json!(rng.pick(&["vault-core", "vault-yield", "x", ""])));
            }
            "data" => {
                if let Some(data) = event.get_mut("data").and_then(Value::as_object_mut) {
                    let keys: Vec<String> = data.keys().cloned().collect();
                    if !keys.is_empty() {
                        let key = rng.pick(&keys).clone();
                        data.insert(key, json!(PLACEHOLDER));
                    }
                }
            }
            _ => {
                event.insert(field.into(), json!(PLACEHOLDER));
            }
        }
    } else {
        value = json!({"reorg": {"from_block": PLACEHOLDER}});
    }
    let placeholder = json!(PLACEHOLDER).to_string();
    value.to_string().replace(&placeholder, odd).into_bytes()
}

fn random_policy(rng: &mut Rng) -> String {
    let mut policy = serde_json::Map::new();
    for vault in ["vault-core", "vault-yield", "vault-legacy", "x"] {
        if rng.chance(70) {
            let mut targets = serde_json::Map::new();
            for strategy in ["aave", "spark", "morpho", "ether-fi"] {
                if rng.chance(50) {
                    let bps = [
                        json!(0),
                        json!(2500),
                        json!(5000),
                        json!(10000),
                        json!(20000),
                        json!(-5),
                        json!(1.5),
                    ];
                    targets.insert(strategy.into(), rng.pick(&bps).clone());
                }
            }
            policy.insert(vault.into(), Value::Object(targets));
        }
    }
    Value::Object(policy).to_string()
}

#[test]
fn randomly_broken_input_never_panics() {
    let lines = sample_lines();
    let mut rng = Rng(0x5eed_cafe_f00d_1234);
    for _ in 0..CASES {
        let mut feed: Vec<Vec<u8>> = Vec::new();
        for _ in 0..1 + rng.below(60) {
            let line = rng.pick(&lines).clone();
            feed.push(if rng.chance(30) { damage(&mut rng, &line) } else { line.into_bytes() });
            if rng.chance(10) {
                let duplicate = rng.pick(&feed).clone();
                feed.push(duplicate);
            }
            if rng.chance(5) {
                feed.push(json!({"reorg": {"from_block": rng.below(25)}}).to_string().into_bytes());
            }
        }
        // Shuffle (Fisher-Yates).
        for i in (1..feed.len()).rev() {
            feed.swap(i, rng.below(i + 1));
        }
        let input = feed.join(&b'\n');

        let engine = ingest(input.as_slice()).expect("reading a byte slice can't fail");
        let rendered = report(&engine, &random_policy(&mut rng).parse()).to_string();
        assert!(!rendered.is_empty());
    }
}
