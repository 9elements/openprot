// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Kernel-tagged IPC runtime for the OpenPRoT boot orchestrator service.
//!
//! The **only** kernel-tagged crate of the service: it wraps the host-buildable
//! [`boot_orchestrator_server`] logic and the pure [`Orchestrator`] state
//! machine in the Pigweed `WaitGroup`/`object_wait` loop, mirroring the
//! topology-agnostic loop of [`i2c_server_runtime`](../../i2c/server-runtime).
//!
//! At boot, [`run`] builds the platform trust [`Chain`], constructs the
//! [`Orchestrator`], and drives the initial power-on event through a
//! [`Platform`] implementation ([`Board`]) before entering the event loop. The
//! IPC event loop itself is currently a stub: no wire protocol
//! ([`boot_orchestrator_api`]) is defined yet, so incoming channel traffic is
//! read and dropped. It is here so the channel plumbing can grow in place.

#![no_std]

use boot_orchestrator_server::{
    Chain, ComponentAttrs, ComponentId, Effect, EffectError, Event, Orchestrator, Platform,
    PowerOnResult,
};
use userspace::syscall::{self, Signals};
use userspace::time::Instant;

/// Trust-chain capacity: the maximum number of components the orchestrator
/// walks. Sized for a placeholder single-component chain.
const CHAIN_CAP: usize = 3;

/// Effect-buffer capacity. The state machine requires `E >= 2 * N + 2`; see
/// [`Orchestrator`].
const EFFECT_CAP: usize = 2 * CHAIN_CAP + 2;

/// A placeholder [`Platform`] for the boot orchestrator.
///
/// A real board drives reset lines, reads/verifies firmware, and reports the
/// outcome back as [`Event`]s. This stub logs each requested [`Effect`] and
/// reports success so the state machine can make progress; replace it with the
/// board-specific actuation layer (e.g. wiring
/// [`fwmanager_api`](../../fwmanager/api)'s `BootControl`/`BootMonitor`).
pub struct Board;

impl Platform for Board {
    fn execute(&mut self, _effect: Effect) -> Result<(), EffectError> {
        // A real board would actuate reset lines / verify firmware here and
        // report the outcome back as `Event`s. The stub accepts every effect.
        pw_log::info!("boot_orchestrator: effect executed");
        Ok(())
    }
}

/// Service entry point: bring up the orchestrator, deliver the initial
/// power-on event, then service the (stub) IPC event loop forever.
///
/// `wg` is the wait-group handle generated for this app from its `system.json5`
/// (see the i2c server for the target-wiring pattern).
pub fn run(wg: u32) -> ! {
    // Build the platform trust chain. `Orchestrator::new` requires a validated
    // `Chain`, so build a `heapless::Vec` of `(ComponentId, ComponentAttrs)`
    // and convert it with `try_into` (the README's raw-`Vec` form is stale).
    let mut chain: heapless::Vec<(ComponentId, ComponentAttrs), CHAIN_CAP> = heapless::Vec::new();
    let _ = chain.push((ComponentId::new(0), ComponentAttrs::active_required()));

    let chain: Chain<CHAIN_CAP> = match chain.try_into() {
        Ok(c) => c,
        Err(_) => {
            pw_log::error!("boot_orchestrator: invalid trust chain");
            loop {}
        }
    };

    // The provisioning bring-up: construct the orchestrator and drive the
    // initial power-on event through the platform.
    let mut orch = Orchestrator::<CHAIN_CAP, EFFECT_CAP>::new(chain, /*max_retry=*/ 3);
    let mut board = Board;

    orch.dispatch(&mut board, Event::PowerGood(PowerOnResult::Provisioned));

    // Stub IPC event loop. Future work: register the service's channel with the
    // wait group, decode `boot_orchestrator_api` requests into `Event`s, and
    // route them through `orch.dispatch(&mut board, event)`.
    let wait_mask = Signals::READABLE;
    let mut request_buf = [0u8; 64];
    loop {
        let Ok(w) = syscall::object_wait(wg, wait_mask, Instant::MAX) else {
            continue;
        };
        if !w.pending_signals.contains(Signals::READABLE) {
            continue;
        }
        let channel = w.user_data as u32;
        // Drain and drop the request until a wire protocol is defined.
        let _ = syscall::channel_read(channel, 0, &mut request_buf);
    }
}
