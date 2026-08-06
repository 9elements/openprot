// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Wire protocol for the OpenPRoT boot orchestrator service.
//!
//! This is the seam crate for the boot orchestrator IPC service. It holds the
//! request/response header types and opcode/error enums that the `server` and
//! `client` sides marshal against, mirroring the layering used by
//! [`services/i2c/api`](../../i2c/api). It is a host-buildable, dependency-free
//! leaf: it must never pull in `pw_kernel`/`userspace`.
//!
//! The wire protocol is deliberately left as a minimal placeholder for now —
//! the boot orchestrator's event/effect surface (see
//! [`openprot_orchestrator_sm`]) is not yet exposed over IPC. The `Op` and
//! `Error` enums below establish the shape the protocol will grow into.

#![no_std]

/// Boot orchestrator wire opcodes.
///
/// Placeholder set — extend with concrete request kinds (e.g. "deliver event",
/// "query state") as the IPC surface is defined.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Op {
    /// No-op / reserved opcode.
    Nop = 0x00,
}

impl TryFrom<u8> for Op {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(Op::Nop),
            _ => Err(Error::InvalidOperation),
        }
    }
}

/// Boot orchestrator wire errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Error {
    /// The request named an opcode the server does not implement.
    InvalidOperation = 0x01,
    /// Catch-all for an error the server could not classify.
    InternalError = 0xFF,
}
