// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Pure dispatch logic for the OpenPRoT boot orchestrator service.
//!
//! Host-buildable and unit-testable: this crate holds no IPC or syscall code
//! (that lives in `server-runtime`). It wraps the pure state machine in
//! [`openprot_orchestrator_sm`] and re-exports the types the runtime and any
//! host test need to construct and drive an [`Orchestrator`], mirroring the
//! `api`/`server`/`server-runtime` split used by
//! [`services/i2c`](../../i2c).
//!
//! The wire `dispatch` entry point is a placeholder until the boot
//! orchestrator's IPC protocol (see [`boot_orchestrator_api`]) is defined.

#![no_std]

pub use openprot_orchestrator_sm::{
    Chain, ComponentAttrs, ComponentId, Effect, EffectError, Event, Orchestrator, Platform,
    PowerOnResult, State,
};

/// Decode one wire request, act on it, and encode the response.
///
/// Placeholder: no wire protocol is defined yet, so every request is rejected
/// with [`boot_orchestrator_api::Error::InvalidOperation`]. Returns the number
/// of bytes written to `response`.
pub fn dispatch(_request: &[u8], _response: &mut [u8]) -> usize {
    // TODO: decode a boot_orchestrator_api request, translate it into an
    // `Event`, drive the owned `Orchestrator`, and encode the resulting state.
    0
}
