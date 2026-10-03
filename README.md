# Curator vault manager: submission

Needs a recent stable Rust (edition 2024; developed on 1.99.0).
`rust-toolchain.toml` selects the stable channel with rustfmt and clippy.
Tested on macOS and Ubuntu.

```bash
cargo run     # reads data/events.jsonl and data/policy.json, prints state and plans
cargo test    # unit and integration tests
```

Or with `make` (`make help` lists every target):

```bash
make run      # same as cargo run
make check    # formatting, clippy, unit and integration tests
```

Or with Docker, without installing Rust:

```bash
make docker-build   # or: docker build -t curator-manager .
make docker-run     # or: docker run --rm curator-manager
```

CI (`.github/workflows/ci.yml`) runs the same `make` targets on Ubuntu and
macOS, in debug and release, and builds and runs the Docker image.

Each vault is shown as planned, or not planned with the reasons why. In the
given data `vault-legacy` has a deposit but no `VaultCreated`, and no policy
entry, so it is not planned, while `vault-core` and `vault-yield` are
planned in full.

Design decisions, assumptions, limitations, tests and the Part 2 write-up are
in [NOTES.md](NOTES.md). The original brief follows, unchanged.

---

# Curator vault management system

We are a curator for a set of onchain vaults (two at the moment). Our job is to
manage the funds in each of these vaults, and how the funds in a vault are
allocated.

A vault takes deposits of one asset and issues shares against those deposits.
The pooled assets are lent out across strategies, each with a cap on how much of
the vault may sit in it.

Yield accrues to the vault, which raises the value of every share without
minting new ones, as is standard for vaults.

We are given a `policy` and need to build an automated system that ingests
onchain events and then derives a plan for how to move funds.

## What you are given

**`data/events.jsonl`** is the feed from an indexer, one JSON delivery per line,
in the order it was delivered.

Everything you know about the chain comes from here, as a raw event log with a
reorg signal.

- The indexer resolves the canonical chain and tells you when a reorg occurs. A
  reorg with `from_block: N` means every event in block `N` or later is no
  longer canonical. The chain may then deliver new events for those blocks.
- The feed reaches you over an append-only queue, **at least once** and in no
  guaranteed order. Delivery is roughly in order on a best-effort basis.
- An event is identified on chain by its `block` and `log_index`.

The event kinds are below. Every amount is an integer in the vault asset's
smallest unit, so with 6 decimals `1000000` is one whole token.

| Kind           | Meaning                                                              |
| -------------- | -------------------------------------------------------------------- |
| `VaultCreated` | A vault exists, with the given asset `decimals`.                     |
| `Deposit`      | `user` paid in `assets` and was minted `shares`.                     |
| `Withdraw`     | `user` burned `shares` and was paid `assets` out of idle.            |
| `Accrue`       | The vault earned `assets` of yield, which lands in idle.             |
| `SetCap`       | The most the vault may hold in `strategy` is now `cap`.              |
| `Allocate`     | `assets` moved from idle into `strategy`.                            |
| `Deallocate`   | `assets` moved from `strategy` back to idle.                         |

The share amounts on `Deposit` and `Withdraw` are what the chain minted and
burned, rounding included, so take them as given.

**`data/policy.json`** is what the curator defined as the allocation each
strategy should hold in each vault, in basis points of the vault's assets. It is
off-chain "ideal" configuration rather than chain state, and drift from it is
expected.

## What to build

A program that reads both files and reports two things.

**The state per vault**: its total assets, shares outstanding, what each holder
is worth, how much sits in each strategy, how much is idle, and so on.

**The plan**: an ordered list of moves, each saying where the capital comes
from, where it goes, and how much, to move the onchain funds towards the policy
the curator has defined.

`cargo run` should print both. How you present them is up to you. Debug output
or logs are fine.

## What makes a plan correct

- Assets are conserved. A move shifts capital between the places it can sit and
  neither creates nor destroys it.
- No strategy finishes above its cap. A strategy with no cap set on chain has an
  effective cap of zero and cannot be funded at all.
- A move can only take what its source holds by the time the plan reaches it.
  Your moves run in the order you write them.

Part of the policy may not be satisfiable. Leaving that part alone is the right
answer. Bending a cap to reach a target is not.

## Ground rules

- Use Rust, starting from the scaffold in this repo.
- Add, remove or restructure anything, dependencies included.
- Aim for at most a few hours.
- A small solution of high quality that you understand is the goal.
- Use AI tooling if you like, but you should understand the code and the
  decisions in it.

Send back the repo, plus a short `NOTES.md` covering three things.

- What you would do next, given more time.
- What you deliberately did not handle, and why.
- How you convinced yourself the state and the plan are correct.
