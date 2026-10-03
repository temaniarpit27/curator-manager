use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use curator_manager::policy::Policy;
use curator_manager::run::{ingest, report};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: curator-manager [--events <path>] [--policy <path>]";

const HELP: &str = "\
Reads an indexer feed and a curator policy, prints each vault's state, and
plans moves towards the policy without breaking any cap.

usage: curator-manager [--events <path>] [--policy <path>]

  --events <path>   the event feed, one JSON delivery per line
                    (default: data/events.jsonl)
  --policy <path>   the curator policy (default: data/policy.json)
  -h, --help        show this help

The report goes to stdout. Logs go to stderr; set RUST_LOG=info to see one
line per event.
";

/// Reads the policy, streams the feed, then reports and plans every vault.
fn main() -> anyhow::Result<()> {
    let Some((events, policy_path)) = parse_args()? else {
        write_to(io::stdout().lock(), HELP).context("writing help")?;
        return Ok(());
    };
    let invalid_filter = init_logging();
    // ERROR level, so every printed log line carries the run ID.
    let _run = tracing::error_span!("run", run_id = %run_id()).entered();
    if let Some(error) = invalid_filter {
        tracing::warn!(%error, "RUST_LOG is invalid; using the default level, warn");
    }

    // The policy first, so a broken one is reported before any feed work.
    let policy = fs::read_to_string(&policy_path)
        .with_context(|| format!("reading {}", policy_path.display()))?
        .parse::<Policy>();
    if let Err(e) = &policy {
        tracing::error!(error = %e, "policy rejected; no vault will be planned");
    }

    let reading = || format!("reading {}", events.display());
    let file = File::open(&events).with_context(reading)?;
    let engine = ingest(BufReader::new(file)).with_context(reading)?;
    let report = report(&engine, &policy);

    write_to(io::stdout().lock(), &report).context("writing report")?;
    Ok(())
}

/// The feed and policy paths, defaulting to the supplied fixture, or `None`
/// for `--help`. Read as `OsString`s, so a non-UTF-8 path is still a path,
/// never a panic.
fn parse_args() -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
    let mut events = PathBuf::from("data/events.jsonl");
    let mut policy = PathBuf::from("data/policy.json");
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let target = match flag.to_str() {
            Some("--events") => &mut events,
            Some("--policy") => &mut policy,
            Some("-h" | "--help") => return Ok(None),
            _ => bail!("unknown argument {}\n{USAGE}", flag.display()),
        };
        let Some(path) = args.next() else { bail!("{} needs a path\n{USAGE}", flag.display()) };
        *target = path.into();
    }
    Ok(Some((events, policy)))
}

/// Logs go to stderr, filtered by `RUST_LOG` (default `warn`). Returns the
/// error if `RUST_LOG` is set but invalid.
fn init_logging() -> Option<String> {
    let (filter, invalid) = match EnvFilter::try_from_default_env() {
        Ok(filter) => (filter, None),
        Err(e) => (EnvFilter::new("warn"), std::env::var_os("RUST_LOG").map(|_| e.to_string())),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .try_init();
    invalid
}

/// Start time and process ID, in hex: unique enough to correlate logs.
fn run_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", std::process::id())
}

/// Write and flush. Used instead of `println!`, which panics on a closed
/// pipe; a closed pipe just means the reader has seen enough.
fn write_to(mut out: impl Write, text: impl Display) -> io::Result<()> {
    match write!(out, "{text}").and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}
