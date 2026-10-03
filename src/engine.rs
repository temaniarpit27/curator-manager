//! Ingestion: turns the stream of deliveries into each vault's state.
//!
//! Events are applied as they arrive. A late event or a reorg rewinds the
//! vault to its last checkpoint before the affected block and replays from
//! there. An event that can't be applied stops the vault until an earlier
//! event fixes it. Input that can't be trusted holds the vault's plan.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use thiserror::Error;
use tracing::{debug, error_span, info, warn};

use crate::event::{Delivery, Event, ParseError, Scope};
use crate::types::{BlockNumber, EventId, VaultId};
use crate::vault::{ApplyError, Vault};

/// Blocks between checkpoints.
const CHECKPOINT_INTERVAL: u64 = 10;

/// What the engine did with one delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Applied,
    /// Newest for its vault, but couldn't be applied.
    Unresolved,
    /// Kept behind an unresolved event.
    Queued,
    /// A late event: restored from the checkpoint at `from` (or from empty)
    /// and replayed.
    Recovered {
        from: Option<BlockNumber>,
        replayed: usize,
    },
    Duplicate,
    Conflict,
    Unknown,
    /// `scoped`: the line still named its vault.
    Malformed {
        scoped: bool,
    },
    Reorg {
        orphaned: usize,
    },
}

/// Where and why a vault's projection stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub event: EventId,
    pub error: ApplyError,
}

/// The vault's state after every event up to the block it's stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkpoint {
    last_applied: Option<EventId>,
    state: Vault,
}

/// One vault: its accepted history and the state derived from it.
#[derive(Debug, Clone, Default)]
pub struct Projection {
    history: BTreeMap<EventId, Event>,
    state: Vault,
    last_applied: Option<EventId>,
    checkpoints: BTreeMap<BlockNumber, Checkpoint>,
    unresolved: Option<Unresolved>,
}

impl Projection {
    pub fn state(&self) -> &Vault {
        &self.state
    }

    pub fn unresolved(&self) -> Option<&Unresolved> {
        self.unresolved.as_ref()
    }

    fn on_insert(&mut self, id: EventId) -> Action {
        // A checkpoint at or after this block may now be missing this event.
        self.invalidate_from(id.block);
        let is_newest = self.last_applied.is_none_or(|last| id > last);
        match &self.unresolved {
            None if is_newest => {
                self.apply_next(id);
                if self.unresolved.is_some() { Action::Unresolved } else { Action::Applied }
            }
            // After the failing event, so it can't fix it.
            Some(u) if id > u.event => Action::Queued,
            // Late, or earlier than the failure: restore and replay.
            _ => {
                let recovery = self.recover_before(id.block);
                Action::Recovered { from: recovery.from, replayed: recovery.replayed }
            }
        }
    }

    /// Apply the next event in chain order, checkpointing at a boundary first.
    fn apply_next(&mut self, id: EventId) {
        self.checkpoint_if_crossing(id.block);
        let Some(event) = self.history.get(&id) else { return };
        match self.state.apply(event) {
            Ok(()) => self.last_applied = Some(id),
            Err(error) => {
                debug!(vault = %event.vault, event = %id, %error, "vault unresolved at this event");
                self.unresolved = Some(Unresolved { event: id, error });
            }
        }
    }

    /// Record a checkpoint at the first boundary between the last applied
    /// block and `block`. If several are crossed, the blocks between are
    /// empty, so the lowest checkpoint covers them all.
    fn checkpoint_if_crossing(&mut self, block: BlockNumber) {
        let Some(last) = self.last_applied else { return };
        let step = CHECKPOINT_INTERVAL;
        // Block 0 isn't a boundary, so after it the first one is `step`.
        let Some(boundary) = last.block.max(1).div_ceil(step).checked_mul(step) else { return };
        if boundary >= block || self.checkpoints.contains_key(&boundary) {
            return;
        }
        self.checkpoints.insert(boundary, self.snapshot());
    }

    /// At the end of input, checkpoint the tip if it is on a boundary.
    fn checkpoint_tip(&mut self) {
        let Some(last) = self.last_applied else { return };
        if self.unresolved.is_some() || last.block == 0 || last.block % CHECKPOINT_INTERVAL != 0 {
            return;
        }
        let checkpoint = self.snapshot();
        self.checkpoints.entry(last.block).or_insert(checkpoint);
    }

    fn snapshot(&self) -> Checkpoint {
        Checkpoint { last_applied: self.last_applied, state: self.state.clone() }
    }

    fn invalidate_from(&mut self, block: BlockNumber) {
        self.checkpoints.split_off(&block);
    }

    /// Restore the last checkpoint before `block` (or the empty state) and
    /// replay forward, stopping at the first event that can't be applied.
    fn recover_before(&mut self, block: BlockNumber) -> Recovery {
        let restored = self.checkpoints.range(..block).next_back().map(|(b, c)| (*b, c.clone()));
        let from = restored.as_ref().map(|(b, _)| *b);
        match restored {
            Some((_, c)) => {
                self.state = c.state;
                self.last_applied = c.last_applied;
            }
            None => {
                self.state = Vault::default();
                self.last_applied = None;
            }
        }
        self.unresolved = None;
        let start = match from {
            None => EventId { block: 0, log_index: 0 },
            Some(b) => match b.checked_add(1) {
                Some(next) => EventId { block: next, log_index: 0 },
                None => return Recovery { from, replayed: 0 }, // nothing comes after the last block
            },
        };
        let pending: Vec<EventId> = self.history.range(start..).map(|(id, _)| *id).collect();
        let mut replayed = 0;
        for id in pending {
            self.apply_next(id);
            if self.unresolved.is_some() {
                break;
            }
            replayed += 1;
        }
        Recovery { from, replayed }
    }

    /// Remove every event from `cut` on, then restore and replay.
    fn truncate(&mut self, cut: EventId) -> (Vec<Event>, Option<Recovery>) {
        let removed = self.history.split_off(&cut);
        let had_checkpoints = self.checkpoints.range(cut.block..).next().is_some();
        self.invalidate_from(cut.block);
        let restored =
            (!removed.is_empty() || had_checkpoints).then(|| self.recover_before(cut.block));
        (removed.into_values().collect(), restored)
    }
}

/// What a restore-and-replay did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Recovery {
    /// `None`: replayed from the empty state.
    from: Option<BlockNumber>,
    replayed: usize,
}

/// A delivery worth reporting. Kept for the whole run, even after a reorg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    Conflict {
        line: usize,
        kept: Event,
        ignored: Event,
    },
    UnknownKind {
        line: usize,
        id: EventId,
        vault: VaultId,
        kind: String,
    },
    Malformed(ParseError),
    OrphanRedelivered {
        line: usize,
        id: EventId,
        vault: VaultId,
    },
    /// `held`: the vaults it changed, now ambiguous (empty if harmless).
    RepeatedReorg {
        line: usize,
        from_block: BlockNumber,
        held: Vec<VaultId>,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { line, kept, .. } => write!(
                f,
                "line {line}: event {} already accepted with different content; not applied",
                kept.id
            ),
            Self::UnknownKind { line, id, vault, kind } => {
                write!(f, "line {line}: {vault} event {id} has unknown kind {kind:?}; not applied")
            }
            Self::Malformed(e) => match &e.scope {
                Scope::Event(vault, id) => {
                    write!(
                        f,
                        "line {}: malformed {vault} event {id}: {}; not applied",
                        e.line, e.reason
                    )
                }
                Scope::Vault(vault) => {
                    write!(f, "line {}: malformed {vault} event: {}; not applied", e.line, e.reason)
                }
                Scope::Unknown => write!(f, "line {}: malformed: {}; skipped", e.line, e.reason),
            },
            Self::OrphanRedelivered { line, id, vault } => {
                write!(
                    f,
                    "line {line}: {vault} event {id} matches one removed by a reorg; accepted"
                )
            }
            Self::RepeatedReorg { line, from_block, .. } => {
                write!(f, "line {line}: reorg from block {from_block} delivered again; applied")
            }
        }
    }
}

/// Why a vault's input can't be trusted; each one withholds its plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InputIssue {
    #[error(
        "line {}: event {id} could not be used (malformed: {}) and no valid copy has arrived",
        error.line,
        error.reason
    )]
    Unconfirmed { id: EventId, error: ParseError },
    /// No event key, so nothing can clear it.
    #[error(
        "line {}: an event for this vault could not be read ({}); its effect is unknown",
        error.line,
        error.reason
    )]
    Unreadable { error: ParseError },
    #[error("line {line}: two different payloads for event {id}; which is canonical is unknown")]
    Conflict { line: usize, id: EventId },
    #[error(
        "line {line}: event {id} was removed by a reorg and delivered again; old or new branch is unknown"
    )]
    OrphanRedelivered { line: usize, id: EventId },
    #[error(
        "line {line}: reorg from block {from_block} delivered again; a redelivery or a second reorg is unknown"
    )]
    RepeatedReorg { line: usize, from_block: BlockNumber },
}

/// A malformed delivery that named a vault and event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unconfirmed {
    vault: VaultId,
    id: EventId,
    error: ParseError,
}

/// Counts over every delivery, including ones a reorg later undid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub deliveries: usize,
    pub accepted: usize,
    pub duplicates: usize,
    pub conflicts: usize,
    pub unknown: usize,
    /// Malformed lines that name no vault.
    pub unreadable: usize,
    /// Malformed lines that still name their vault (bad data, or a broken
    /// envelope with a vault and a supported kind).
    pub bad_data: usize,
    pub reorgs: usize,
    pub orphaned: usize,
}

#[derive(Debug, Default)]
pub struct Engine {
    /// Every accepted key (chain-wide) and its vault.
    keys: BTreeMap<EventId, VaultId>,
    vaults: BTreeMap<VaultId, Projection>,
    /// Events removed by reorgs, to spot one delivered again.
    orphaned: Vec<Event>,
    /// Heights already cut by a reorg, to spot a repeated signal.
    reorg_heights: BTreeSet<BlockNumber>,
    /// Malformed events, each holding its vault until a valid copy arrives.
    unconfirmed: Vec<Unconfirmed>,
    warnings: Vec<Warning>,
    stats: Stats,
}

impl Engine {
    /// Handle one decoded delivery; `line` is its line in the feed.
    pub fn ingest(&mut self, line: usize, delivery: Delivery) -> Action {
        // ERROR level so warnings still carry the line under the default filter.
        let _span = error_span!("delivery", line).entered();
        let action = match delivery {
            Delivery::Event(event) => self.on_event(line, event),
            Delivery::Reorg { from_block } => self.on_reorg(line, from_block),
            Delivery::Unsupported { id, vault, kind } => {
                // Not held: assumed to be a vault's own extension.
                warn!(line, event = %id, %vault, %kind, "unknown event kind; skipped");
                self.warnings.push(Warning::UnknownKind { line, id, vault, kind });
                Action::Unknown
            }
        };
        self.count(action);
        action
    }

    /// Handle a line that couldn't be decoded. It holds its vault if it
    /// names one.
    pub fn reject(&mut self, error: ParseError) -> Action {
        let line = error.line;
        let _span = error_span!("delivery", line).entered();
        warn!(line, reason = %error.reason, "malformed line; skipped");
        let scoped = error.scope != Scope::Unknown;
        if let Scope::Event(vault, id) = &error.scope {
            let (vault, id) = (vault.clone(), *id);
            self.unconfirmed.push(Unconfirmed { vault, id, error: error.clone() });
        }
        self.warnings.push(Warning::Malformed(error));
        let action = Action::Malformed { scoped };
        self.count(action);
        action
    }

    fn count(&mut self, action: Action) {
        let s = &mut self.stats;
        s.deliveries += 1;
        match action {
            Action::Applied | Action::Unresolved | Action::Queued | Action::Recovered { .. } => {
                s.accepted += 1
            }
            Action::Duplicate => s.duplicates += 1,
            Action::Conflict => s.conflicts += 1,
            Action::Unknown => s.unknown += 1,
            Action::Malformed { scoped: false } => s.unreadable += 1,
            Action::Malformed { scoped: true } => s.bad_data += 1,
            Action::Reorg { orphaned } => {
                s.reorgs += 1;
                s.orphaned += orphaned;
            }
        }
    }

    /// End of input: record tip checkpoints.
    pub fn finish(&mut self) {
        for projection in self.vaults.values_mut() {
            projection.checkpoint_tip();
        }
    }

    fn on_event(&mut self, line: usize, event: Event) -> Action {
        if let Some(kept) = self.accepted_event(&event.id) {
            if *kept == event {
                info!(line, event = %event.id, "duplicate ignored");
                return Action::Duplicate;
            }
            warn!(line, event = %event.id, "key already used by a different event; later delivery ignored");
            let kept = kept.clone();
            self.warnings.push(Warning::Conflict { line, kept, ignored: event });
            return Action::Conflict;
        }
        if self.orphaned.contains(&event) {
            warn!(line, event = %event.id, "matches an event removed by a reorg; accepted, vault marked ambiguous");
            let (id, vault) = (event.id, event.vault.clone());
            self.warnings.push(Warning::OrphanRedelivered { line, id, vault });
        }

        let (id, vault) = (event.id, event.vault.clone());
        self.keys.insert(id, vault.clone());
        let projection = self.vaults.entry(vault.clone()).or_default();
        projection.history.insert(id, event);
        let was_unresolved = projection.unresolved.clone();
        let action = projection.on_insert(id);
        let now_unresolved = projection.unresolved.clone();
        info!(line, event = %id, %vault, ?action, "event accepted");
        log_resolution(&vault, was_unresolved, now_unresolved, Cause::Arrival);
        action
    }

    fn on_reorg(&mut self, line: usize, from_block: BlockNumber) -> Action {
        let cut = EventId { block: from_block, log_index: 0 };
        let repeated = !self.reorg_heights.insert(from_block);
        let mut orphaned = 0;
        let mut transitions = Vec::new();
        // Vaults this signal changes, which matters if it is a repeat.
        let mut changed = BTreeSet::new();
        for (vault, projection) in &mut self.vaults {
            let was_unresolved = projection.unresolved.clone();
            let (removed, _) = projection.truncate(cut);
            transitions.push((vault.clone(), was_unresolved, projection.unresolved.clone()));
            if !removed.is_empty() {
                changed.insert(vault.clone());
            }
            for event in removed {
                orphaned += 1;
                self.keys.remove(&event.id);
                self.orphaned.push(event);
            }
        }
        // A vault whose every event was orphaned never existed on this chain.
        self.vaults.retain(|_, p| !p.history.is_empty());
        for (vault, was, now) in transitions {
            log_resolution(&vault, was, now, Cause::Reorg);
        }
        // A malformed event at a cut block was on the old branch.
        for u in self.unconfirmed.iter().filter(|u| u.id.block >= from_block) {
            changed.insert(u.vault.clone());
        }
        self.unconfirmed.retain(|u| u.id.block < from_block);
        // A repeat that changed something may have removed real replacements.
        if repeated {
            warn!(line, from_block, "reorg signal at a height already cut");
            let held = changed.into_iter().collect();
            self.warnings.push(Warning::RepeatedReorg { line, from_block, held });
        }
        info!(line, from_block, orphaned, "reorg applied");
        Action::Reorg { orphaned }
    }

    /// Why this vault's input can't be trusted right now. Ambiguity comes from
    /// the warnings and lasts the whole run; a malformed event counts until a
    /// valid copy is accepted.
    pub fn input_issues(&self, vault: &VaultId) -> Vec<InputIssue> {
        let ambiguous = self.warnings.iter().filter_map(|w| match w {
            Warning::Conflict { line, kept, ignored }
                if &kept.vault == vault || &ignored.vault == vault =>
            {
                Some(InputIssue::Conflict { line: *line, id: kept.id })
            }
            Warning::OrphanRedelivered { line, id, vault: v } if v == vault => {
                Some(InputIssue::OrphanRedelivered { line: *line, id: *id })
            }
            Warning::RepeatedReorg { line, from_block, held } if held.contains(vault) => {
                Some(InputIssue::RepeatedReorg { line: *line, from_block: *from_block })
            }
            Warning::Malformed(e) if e.scope == Scope::Vault(vault.clone()) => {
                Some(InputIssue::Unreadable { error: e.clone() })
            }
            _ => None,
        });
        let accepted =
            |id: &EventId| self.vaults.get(vault).is_some_and(|p| p.history.contains_key(id));
        let pending = self
            .unconfirmed
            .iter()
            .filter(|u| &u.vault == vault && !accepted(&u.id))
            .map(|u| InputIssue::Unconfirmed { id: u.id, error: u.error.clone() });
        ambiguous.chain(pending).collect()
    }

    /// Every vault with an input issue, even one with no accepted event.
    pub fn vaults_with_input_issues(&self) -> impl Iterator<Item = &VaultId> {
        let named: BTreeSet<&VaultId> = self
            .warnings
            .iter()
            .flat_map(|w| match w {
                Warning::Conflict { kept, ignored, .. } => vec![&kept.vault, &ignored.vault],
                Warning::OrphanRedelivered { vault, .. } => vec![vault],
                Warning::RepeatedReorg { held, .. } => held.iter().collect(),
                Warning::Malformed(e) => match &e.scope {
                    Scope::Vault(v) | Scope::Event(v, _) => vec![v],
                    Scope::Unknown => vec![],
                },
                Warning::UnknownKind { .. } => vec![],
            })
            .collect();
        named.into_iter().filter(|v| !self.input_issues(v).is_empty())
    }

    fn accepted_event(&self, id: &EventId) -> Option<&Event> {
        let vault = self.keys.get(id)?;
        self.vaults.get(vault)?.history.get(id)
    }

    pub fn vaults(&self) -> &BTreeMap<VaultId, Projection> {
        &self.vaults
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

/// Log a vault becoming stuck, or no longer stuck.
fn log_resolution(vault: &VaultId, was: Option<Unresolved>, now: Option<Unresolved>, cause: Cause) {
    match (was.is_some(), now) {
        (false, Some(u)) => {
            warn!(%vault, event = %u.event, error = %u.error, "vault unresolved; waiting for an earlier event");
        }
        (true, None) if cause == Cause::Arrival => {
            info!(%vault, "vault resolved by an earlier arrival");
        }
        // Not a resolution: the failing event was removed.
        (true, None) => info!(%vault, "unresolved event removed by a reorg"),
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    Arrival,
    Reorg,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use crate::types::{Assets, Shares};

    fn engine() -> Engine {
        Engine::default()
    }

    fn id(block: u64) -> EventId {
        EventId { block, log_index: 0 }
    }

    fn at(block: u64, log_index: u64, kind: EventKind) -> Event {
        Event { id: EventId { block, log_index }, vault: "v".into(), kind }
    }

    fn ev(block: u64, kind: EventKind) -> Event {
        at(block, 0, kind)
    }

    fn created(block: u64) -> Event {
        ev(block, EventKind::VaultCreated { decimals: 6 })
    }

    fn deposit(block: u64, assets: u128) -> Event {
        ev(
            block,
            EventKind::Deposit {
                user: "u".into(),
                assets: Assets::new(assets),
                shares: Shares::new(assets),
            },
        )
    }

    fn withdraw(block: u64, assets: u128) -> Event {
        ev(
            block,
            EventKind::Withdraw {
                user: "u".into(),
                assets: Assets::new(assets),
                shares: Shares::new(assets),
            },
        )
    }

    fn accrue(block: u64, assets: u128) -> Event {
        ev(block, EventKind::Accrue { assets: Assets::new(assets) })
    }

    fn set_cap(block: u64, cap: u128) -> Event {
        ev(block, EventKind::SetCap { strategy: "s".into(), cap: Assets::new(cap) })
    }

    fn allocate(block: u64, assets: u128) -> Event {
        ev(block, EventKind::Allocate { strategy: "s".into(), assets: Assets::new(assets) })
    }

    fn feed(engine: &mut Engine, events: Vec<Event>) -> Vec<Action> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, e)| engine.ingest(i + 1, Delivery::Event(e)))
            .collect()
    }

    fn reorg(engine: &mut Engine, from_block: u64) -> Action {
        engine.ingest(0, Delivery::Reorg { from_block })
    }

    fn vault(engine: &Engine) -> &Projection {
        &engine.vaults()["v"]
    }

    fn idle(engine: &Engine) -> Assets {
        vault(engine).state().idle
    }

    fn checkpoints(engine: &Engine) -> Vec<u64> {
        vault(engine).checkpoints.keys().copied().collect()
    }

    // ---- ordering and unresolved accounting --------------------------------

    #[test]
    fn allocation_before_its_deposit_resolves_when_the_deposit_arrives() {
        // Fixture lines 12/13.
        let mut e = engine();
        let actions =
            feed(&mut e, vec![created(1), set_cap(2, 500), allocate(9, 240), deposit(8, 300)]);
        assert_eq!(actions[2], Action::Unresolved);
        assert_eq!(actions[3], Action::Recovered { from: None, replayed: 4 });
        assert_eq!(vault(&e).unresolved(), None);
        assert_eq!(idle(&e), Assets::new(60));
        assert_eq!(vault(&e).state().strategies["s"].balance, Assets::new(240));
    }

    #[test]
    fn events_after_an_unresolved_one_are_queued() {
        let mut e = engine();
        let actions =
            feed(&mut e, vec![created(1), withdraw(5, 40), deposit(6, 100), deposit(2, 100)]);
        assert_eq!(
            &actions[1..],
            [Action::Unresolved, Action::Queued, Action::Recovered { from: None, replayed: 4 }]
        );
        assert_eq!(vault(&e).unresolved(), None);
        assert_eq!(idle(&e), Assets::new(160));
    }

    #[test]
    fn impossible_order_that_nets_to_zero_is_caught() {
        // Withdraw at 5 then deposit at 6: totals end at 0, but block 5 can't
        // be applied in chain order.
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(6, 100), withdraw(5, 100)]);
        assert_eq!(vault(&e).unresolved().map(|u| u.event), Some(id(5)));
    }

    #[test]
    fn late_events_within_one_block_are_ordered_by_log_index() {
        let mut e = engine();
        let fund = at(
            4,
            2,
            EventKind::Deposit {
                user: "u".into(),
                assets: Assets::new(50),
                shares: Shares::new(50),
            },
        );
        let spend = at(4, 5, EventKind::Allocate { strategy: "s".into(), assets: Assets::new(50) });
        let actions = feed(&mut e, vec![created(1), set_cap(2, 100), spend, fund]);
        assert_eq!(
            &actions[2..],
            [Action::Unresolved, Action::Recovered { from: None, replayed: 4 }]
        );
        assert_eq!(vault(&e).state().strategies["s"].balance, Assets::new(50));
    }

    // ---- creation ----------------------------------------------------------

    #[test]
    fn events_before_creation_wait_for_an_earlier_creation() {
        let mut e = engine();
        assert_eq!(feed(&mut e, vec![deposit(5, 100)]), [Action::Unresolved]);
        assert_eq!(idle(&e), Assets::ZERO, "nothing applied before creation");
        assert_eq!(feed(&mut e, vec![created(2)]), [Action::Recovered { from: None, replayed: 2 }]);
        assert_eq!(vault(&e).unresolved(), None);
        assert_eq!(idle(&e), Assets::new(100));
    }

    #[test]
    fn a_creation_later_in_chain_order_does_not_resolve_earlier_events() {
        let mut e = engine();
        feed(&mut e, vec![deposit(5, 100), created(9)]);
        let u = vault(&e).unresolved().expect("still unresolved");
        assert_eq!(u.event, id(5));
        assert_eq!(u.error, ApplyError::MissingCreation);
    }

    #[test]
    fn creation_alone_does_not_clear_other_failures() {
        let mut e = engine();
        feed(&mut e, vec![created(1), withdraw(5, 10)]);
        feed(&mut e, vec![created(2)]); // a repeated creation, earlier than the failure
        assert_eq!(vault(&e).unresolved().map(|u| u.event), Some(id(5)));
    }

    // ---- identity ----------------------------------------------------------

    #[test]
    fn same_key_is_first_wins() {
        let mut e = engine();
        let actions =
            feed(&mut e, vec![created(1), deposit(2, 100), deposit(2, 100), deposit(2, 999)]);
        assert_eq!(&actions[1..], [Action::Applied, Action::Duplicate, Action::Conflict]);
        assert_eq!(idle(&e), Assets::new(100));
        assert!(matches!(e.warnings(), [Warning::Conflict { line: 4, .. }]));
    }

    #[test]
    fn the_key_is_chain_wide() {
        let mut e = engine();
        let other_vault = Event {
            id: id(2),
            vault: "w".into(),
            kind: EventKind::Accrue { assets: Assets::new(5) },
        };
        let actions = feed(&mut e, vec![created(1), deposit(2, 100), other_vault]);
        assert_eq!(actions[2], Action::Conflict);
        assert!(!e.vaults().contains_key("w"));
    }

    // ---- skipping ----------------------------------------------------------

    #[test]
    fn malformed_lines_are_skipped_and_reserve_nothing() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100)]);
        let bad = ParseError {
            line: 3,
            scope: Scope::Event("v".into(), id(3)),
            reason: "bad data".into(),
        };
        assert_eq!(e.reject(bad), Action::Malformed { scoped: true });
        assert_eq!((e.stats().bad_data, e.stats().unreadable), (1, 0));
        assert_eq!(vault(&e).unresolved(), None);
        // Its effect is unknown, so the vault can't be planned for now.
        assert!(matches!(e.input_issues(&"v".into())[..], [InputIssue::Unconfirmed { .. }]));
        // A valid delivery of the same key is accepted normally, and clears it.
        assert_eq!(feed(&mut e, vec![deposit(3, 7)]), [Action::Applied]);
        assert_eq!(idle(&e), Assets::new(107));
        assert!(e.input_issues(&"v".into()).is_empty());
        assert!(matches!(e.warnings(), [Warning::Malformed(_)]), "the original warning is kept");
    }

    #[test]
    fn a_malformed_copy_of_an_accepted_event_blocks_nothing() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100)]);
        let bad =
            ParseError { line: 3, scope: Scope::Event("v".into(), id(2)), reason: "x".into() };
        e.reject(bad);
        assert!(e.input_issues(&"v".into()).is_empty(), "a valid copy is already in");
    }

    #[test]
    fn an_unreadable_line_blocks_nothing() {
        let mut e = engine();
        feed(&mut e, vec![created(1)]);
        e.reject(ParseError { line: 2, scope: Scope::Unknown, reason: "not json".into() });
        assert!(e.input_issues(&"v".into()).is_empty());
    }

    #[test]
    fn unknown_kinds_are_skipped() {
        let mut e = engine();
        feed(&mut e, vec![created(1)]);
        let unknown = Delivery::Unsupported { id: id(2), vault: "v".into(), kind: "Rebase".into() };
        assert_eq!(e.ingest(2, unknown), Action::Unknown);
        assert_eq!(vault(&e).history.len(), 1, "it reserves no key");
        assert!(matches!(e.warnings(), [Warning::UnknownKind { .. }]));
        assert!(e.input_issues(&"v".into()).is_empty(), "possibly the vault's own extension");
    }

    #[test]
    fn a_reorg_cutting_a_malformed_line_drops_its_hold() {
        let mut e = engine();
        feed(&mut e, vec![created(1)]);
        let bad =
            ParseError { line: 2, scope: Scope::Event("v".into(), id(2)), reason: "x".into() };
        e.reject(bad);
        assert!(!e.input_issues(&"v".into()).is_empty());
        reorg(&mut e, 2);
        assert!(e.input_issues(&"v".into()).is_empty(), "it belonged to the old branch");
    }

    // ---- checkpoints -------------------------------------------------------

    /// Events in blocks 1, 5, 15, 25, 35, 45: boundaries 10, 20, 30 and 40 are
    /// each crossed by the next event.
    fn ten_block_vault() -> Engine {
        let mut e = engine();
        feed(
            &mut e,
            vec![
                created(1),
                deposit(5, 1),
                deposit(15, 1),
                deposit(25, 1),
                deposit(35, 1),
                deposit(45, 1),
            ],
        );
        e
    }

    #[test]
    fn checkpoints_are_taken_at_ten_block_boundaries() {
        assert_eq!(checkpoints(&ten_block_vault()), [10, 20, 30, 40]);
    }

    #[test]
    fn late_event_in_block_25_restores_checkpoint_20() {
        let mut e = ten_block_vault();
        // Block 25 already has an event at log index 0; this one comes after it.
        let action = feed(&mut e, vec![at(25, 1, EventKind::Accrue { assets: Assets::new(100) })]);
        assert_eq!(action, [Action::Recovered { from: Some(20), replayed: 4 }]);
        assert_eq!(checkpoints(&e), [10, 20, 30, 40], "30 and 40 were rebuilt during replay");
        assert_eq!(idle(&e), Assets::new(105));
    }

    #[test]
    fn late_event_inside_a_boundary_block_restores_the_one_before() {
        let mut e = ten_block_vault();
        assert_eq!(
            feed(&mut e, vec![accrue(20, 7)]),
            [Action::Recovered { from: Some(10), replayed: 5 }]
        );
        assert_eq!(idle(&e), Assets::new(12));
    }

    #[test]
    fn late_event_before_any_checkpoint_replays_from_empty() {
        let mut e = ten_block_vault();
        assert_eq!(
            feed(&mut e, vec![accrue(3, 1)]),
            [Action::Recovered { from: None, replayed: 7 }]
        );
        assert_eq!(idle(&e), Assets::new(6));
    }

    #[test]
    fn sparse_blocks_keep_one_checkpoint_for_the_whole_gap() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(5, 1), deposit(47, 1)]);
        assert_eq!(checkpoints(&e), [10], "10, 20, 30, 40 would all be identical");
        // A late event inside the gap still recovers from it.
        assert_eq!(
            feed(&mut e, vec![accrue(33, 1)]),
            [Action::Recovered { from: Some(10), replayed: 2 }]
        );
        assert_eq!(idle(&e), Assets::new(3));
    }

    #[test]
    fn an_event_in_a_block_covered_by_a_checkpoint_invalidates_it() {
        // The vault halts at block 35 after checkpoint 10 was taken; an event
        // at block 8 lands inside what checkpoint 10 claimed was complete.
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(5, 1), withdraw(35, 50)]);
        assert_eq!(checkpoints(&e), [10]);
        feed(&mut e, vec![deposit(8, 49)]);
        assert_eq!(vault(&e).unresolved(), None);
        assert_eq!(idle(&e), Assets::new(0));
    }

    #[test]
    fn no_checkpoint_past_an_unresolved_event() {
        let mut e = engine();
        feed(&mut e, vec![created(1), withdraw(5, 10), deposit(25, 1), deposit(45, 1)]);
        e.finish();
        assert!(checkpoints(&e).is_empty());
    }

    #[test]
    fn end_of_input_records_a_checkpoint_at_a_boundary_tip() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(20, 1)]);
        assert_eq!(checkpoints(&e), [10]);
        e.finish();
        assert_eq!(checkpoints(&e), [10, 20]);
    }

    #[test]
    fn a_vault_starting_at_block_0_still_gets_its_first_boundary() {
        let mut e = engine();
        feed(&mut e, vec![created(0), deposit(15, 1)]);
        assert_eq!(checkpoints(&e), [10]);
    }

    #[test]
    fn blocks_near_u64_max_neither_wrap_nor_panic() {
        // max - 15 is a boundary; the next one, past max, doesn't fit in u64.
        let mut e = engine();
        let max = u64::MAX;
        feed(&mut e, vec![created(max - 15), deposit(max - 1, 1), deposit(max, 2)]);
        e.finish();
        assert_eq!(checkpoints(&e), [max - 15]);
        let action = e.ingest(0, Delivery::Event(deposit(max - 2, 4)));
        assert_eq!(action, Action::Recovered { from: Some(max - 15), replayed: 3 });
        assert_eq!(idle(&e), Assets::new(7));
        reorg(&mut e, max);
        assert_eq!(idle(&e), Assets::new(5));
    }

    // ---- reorgs ------------------------------------------------------------

    #[test]
    fn reorg_removes_events_creation_and_caps_and_rebuilds() {
        let mut e = engine();
        feed(
            &mut e,
            vec![created(1), deposit(2, 100), set_cap(3, 50), set_cap(25, 10), allocate(26, 10)],
        );
        assert_eq!(reorg(&mut e, 25), Action::Reorg { orphaned: 2 });
        let s = &vault(&e).state().strategies["s"];
        assert_eq!(s.cap, Some(Assets::new(50)), "the cap from block 25 is gone");
        assert_eq!(s.balance, Assets::new(0));
        assert_eq!(idle(&e), Assets::new(100));
        assert_eq!(checkpoints(&e), [10]);

        reorg(&mut e, 1);
        assert!(e.vaults().is_empty(), "with the creation gone, so is the vault");
    }

    #[test]
    fn reorg_at_a_boundary_drops_that_checkpoint() {
        let mut e = ten_block_vault();
        reorg(&mut e, 20);
        assert_eq!(checkpoints(&e), [10]);
        assert_eq!(idle(&e), Assets::new(2));
    }

    #[test]
    fn reorg_clears_an_unresolved_event_it_removes() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 5), withdraw(30, 50)]);
        assert!(vault(&e).unresolved().is_some());
        reorg(&mut e, 30);
        assert_eq!(vault(&e).unresolved(), None);
        assert_eq!(idle(&e), Assets::new(5));
    }

    #[test]
    fn replacement_after_reorg_reuses_the_key() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(15, 1), deposit(16, 500)]);
        reorg(&mut e, 16);
        assert_eq!(feed(&mut e, vec![deposit(16, 50)]), [Action::Applied]);
        assert_eq!(idle(&e), Assets::new(51));
        assert!(e.warnings().is_empty(), "a freed key is not a conflict");
        assert!(e.input_issues(&"v".into()).is_empty(), "the supported order is trusted");
    }

    // ---- detected ambiguity ---------------------------------------------

    #[test]
    fn a_conflict_makes_both_claimed_vaults_ambiguous() {
        let mut e = engine();
        let in_w = |event: Event| Event { vault: "w".into(), ..event };
        feed(&mut e, vec![created(1), in_w(at(1, 1, EventKind::VaultCreated { decimals: 6 }))]);
        feed(&mut e, vec![deposit(2, 50), in_w(deposit(2, 10))]);
        assert_eq!(idle(&e), Assets::new(50), "the first payload is still the one applied");
        for vault in ["v", "w"] {
            let issues = e.input_issues(&vault.into());
            assert!(matches!(issues[..], [InputIssue::Conflict { .. }]), "{vault}: {issues:?}");
        }
    }

    #[test]
    fn a_replacement_before_its_signal_is_caught_as_a_conflict() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(3, 50), deposit(3, 10)]);
        reorg(&mut e, 3);
        assert_eq!(idle(&e), Assets::new(100), "the replacement was lost");
        assert!(matches!(e.input_issues(&"v".into())[..], [InputIssue::Conflict { .. }]));
    }

    #[test]
    fn an_orphan_delivered_again_makes_its_vault_ambiguous() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(3, 50)]);
        reorg(&mut e, 3);
        feed(&mut e, vec![deposit(3, 50)]);
        let issues = e.input_issues(&"v".into());
        assert!(matches!(issues[..], [InputIssue::OrphanRedelivered { .. }]), "{issues:?}");
        assert!(matches!(e.warnings(), [Warning::OrphanRedelivered { .. }]));
    }

    #[test]
    fn a_repeated_signal_that_removes_replacements_makes_the_vault_ambiguous() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(3, 50)]);
        reorg(&mut e, 3);
        feed(&mut e, vec![deposit(3, 10)]);
        reorg(&mut e, 3);
        assert_eq!(idle(&e), Assets::new(100));
        let issues = e.input_issues(&"v".into());
        assert!(matches!(issues[..], [InputIssue::RepeatedReorg { .. }]), "{issues:?}");
    }

    #[test]
    fn a_repeated_signal_that_changes_nothing_is_only_a_warning() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(3, 50)]);
        reorg(&mut e, 3);
        reorg(&mut e, 3);
        feed(&mut e, vec![deposit(3, 10)]);
        assert_eq!(idle(&e), Assets::new(110));
        assert!(e.input_issues(&"v".into()).is_empty());
        assert!(matches!(e.warnings(), [Warning::RepeatedReorg { .. }]));
    }

    #[test]
    fn a_repeated_signal_that_drops_a_hold_makes_the_vault_ambiguous() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(4, 50)]);
        reorg(&mut e, 4);
        // A malformed replacement holds the vault...
        let bad =
            ParseError { line: 5, scope: Scope::Event("v".into(), id(4)), reason: "x".into() };
        e.reject(bad);
        // ...and a repeat of the signal must not quietly release it.
        reorg(&mut e, 4);
        let issues = e.input_issues(&"v".into());
        assert!(matches!(issues[..], [InputIssue::RepeatedReorg { .. }]), "{issues:?}");
    }

    #[test]
    fn a_replacement_before_its_signal_with_no_old_copy_is_not_detectable() {
        // The limit of the feed format: nothing collides, so the
        // signal simply removes the early replacement.
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(4, 10)]);
        reorg(&mut e, 4);
        assert_eq!(idle(&e), Assets::new(100));
        assert!(e.input_issues(&"v".into()).is_empty());
    }

    #[test]
    fn ambiguity_survives_a_later_reorg() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 50), deposit(2, 10)]);
        reorg(&mut e, 2);
        assert!(!e.input_issues(&"v".into()).is_empty(), "only a resync could settle it");
    }

    #[test]
    fn out_of_order_events_before_a_reorg_are_kept_if_below_it() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(15, 1), deposit(14, 2), deposit(16, 500)]);
        reorg(&mut e, 16);
        assert_eq!(idle(&e), Assets::new(3));
    }

    #[test]
    fn warnings_survive_a_reorg() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(20, 1), deposit(20, 2)]);
        reorg(&mut e, 20);
        assert_eq!(e.warnings().len(), 1, "delivery diagnostics are cumulative");
    }

    #[test]
    fn one_reorg_rebuilds_every_affected_vault() {
        let mut e = engine();
        let in_w = |event: Event| Event { vault: "w".into(), ..event };
        feed(
            &mut e,
            vec![
                created(1),
                in_w(at(1, 1, EventKind::VaultCreated { decimals: 6 })),
                deposit(2, 100),
                in_w(deposit(3, 200)),
                deposit(5, 50),
                in_w(deposit(6, 70)),
            ],
        );
        assert_eq!(reorg(&mut e, 4), Action::Reorg { orphaned: 2 });
        feed(&mut e, vec![in_w(deposit(4, 9))]);
        assert_eq!(idle(&e), Assets::new(100));
        assert_eq!(e.vaults()["w"].state().idle, Assets::new(209));
    }

    #[test]
    fn a_second_reorg_cuts_below_the_first_replacements() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 100), deposit(3, 10), deposit(4, 20)]);
        reorg(&mut e, 4);
        feed(&mut e, vec![deposit(4, 7)]);
        assert_eq!(idle(&e), Assets::new(117));
        reorg(&mut e, 3);
        feed(&mut e, vec![deposit(3, 1)]);
        assert_eq!(idle(&e), Assets::new(101), "both blocks 3 and 4 were replaced");
        assert_eq!(e.stats().reorgs, 2);
    }

    // ---- reorg bookkeeping -------------------------------------------------

    #[test]
    fn a_reorg_removing_the_failing_event_is_not_a_resolution() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 10), withdraw(5, 50)]);
        assert!(vault(&e).unresolved().is_some());
        reorg(&mut e, 5);
        assert!(vault(&e).unresolved().is_none());
    }

    #[test]
    fn a_replay_counts_only_the_events_it_applies() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(2, 10), withdraw(5, 50)]);
        // The late deposit replays block 2 and itself; block 5 still fails.
        let action = e.ingest(0, Delivery::Event(deposit(3, 1)));
        assert_eq!(action, Action::Recovered { from: None, replayed: 3 });
    }

    #[test]
    fn a_vault_whose_every_event_is_orphaned_is_removed() {
        let mut e = engine();
        feed(&mut e, vec![created(1), deposit(10, 100)]);
        reorg(&mut e, 1);
        assert!(e.vaults().is_empty());
        feed(&mut e, vec![created(1), deposit(10, 200)]);
        assert_eq!(idle(&e), Assets::new(200), "recreated from the replacement branch only");
    }
}
