// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! [`SelfUpdate`](orchestrator_capabilities::SelfUpdate) for an eRoT whose
//! boot stage reads a record the eRoT writes.
//!
//! The record format, the tried mark, the handover and the slot choice of
//! the boot stage live in `orchestrator_self_update_format`, which the boot
//! stage links too. This crate adds the storage side: it reads record
//! copies A and B at boot, reconciles them with the slot the boot stage
//! started, and writes one copy per transition.
//!
//! The pending mark is part of the record: `set_trial_pending` writes the
//! `TrialPending` phase, and the boot stage reads that phase. The trial
//! watchdog, the lock on the mark region and the handover mechanism belong
//! to the boot stage and the board, not to this crate.

#![cfg_attr(not(test), no_std)]

mod session;
#[cfg(test)]
mod tests;

pub use session::{Found, RecordCopies, RecordError, RecordSession, Rewrite};
