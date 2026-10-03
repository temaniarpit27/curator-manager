//! Curator vault manager: reads an indexer feed line by line, rebuilds each
//! vault's state, and plans moves towards the curator's policy without
//! breaking any cap.
//!
//! `feed` parses a line into an [`event`]; the [`engine`] applies it using
//! [`vault`]; [`run`] decides which vaults to plan with [`planner`] and
//! [`policy`]; [`report`] holds and prints the result.

pub mod types;
