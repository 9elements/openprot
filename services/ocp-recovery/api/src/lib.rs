// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Register layouts of the OCP Recovery interface.
//!
//! A Recovery Agent drives the registers over SMBus, I3C or USB. The side
//! being recovered answers, and the recovery spec calls that side the
//! Device. The Recovery Agent code and the device code both use this crate.
//! The crate holds the command codes, the byte layouts, the field
//! encodings, the payload caps and the rules for which accesses the device
//! refuses. The transport framing, such as the
//! SMBus PEC, and the storage of the register values stay outside, so the
//! crate builds on the host and needs no kernel.
//!
//! [`RecoveryCommand`] decodes the command byte once and carries the access
//! rule, the length and the availability scope with it.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// A test unwraps the result of a call it set up, so a panic marks a test bug.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod command;
pub mod error;
pub mod regs;

pub use command::{Access, Length, RecoveryCommand, Scope, Transport};
pub use error::WireError;
pub use regs::{Capabilities, ProtocolError};

/// Largest payload of `INDIRECT_DATA` and `INDIRECT_FIFO_DATA` in 1 SMBus
/// access, as section 9.2 of the spec states. For I3C and USB, the caller
/// passes the cap in [`Transport::Negotiated`].
pub const SMBUS_MAX_DATA_PAYLOAD: usize = 252;
