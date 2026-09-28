use std::{error::Error, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let feed = fs::read_to_string("data/events.jsonl")?;
    let policy = fs::read_to_string("data/policy.json")?;

    // TODO: fold the feed into per-vault state, report it, then plan the
    // moves that bring each vault towards the policy.
    println!("{} deliveries, {} bytes of policy", feed.lines().count(), policy.len());

    Ok(())
}
