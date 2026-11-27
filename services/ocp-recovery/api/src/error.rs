// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Errors the crate returns for an access from the Recovery Agent.

use crate::command::RecoveryCommand;
use crate::regs::ProtocolError;

/// Why the device refuses an access from the Recovery Agent.
///
/// Each variant maps to one of the protocol error codes the spec defines for
/// byte 1 of `DEVICE_STATUS`, so a handler converts with `ProtocolError::from`
/// and reports the result without a second match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The command byte names no recovery command.
    UnknownCommand(u8),
    /// The Recovery Agent wrote a command the device serves read-only.
    WriteToReadOnly(RecoveryCommand),
    /// A command of scope R ([`Scope::InterfaceActive`]) arrived while
    /// `DEVICE_STATUS` byte 0 read 0.
    ///
    /// [`Scope::InterfaceActive`]: crate::command::Scope::InterfaceActive
    NotServedInState(RecoveryCommand),
    /// The write carried the wrong number of bytes for the command.
    LengthMismatch {
        /// The command the Recovery Agent addressed.
        command: RecoveryCommand,
        /// How many bytes arrived.
        got: usize,
    },
    /// A field held a value the spec reserves or does not define.
    UnsupportedParameter {
        /// The command the Recovery Agent addressed.
        command: RecoveryCommand,
        /// Which byte of the register carried the value.
        offset: usize,
        /// The value that arrived.
        value: u8,
    },
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownCommand(byte) => write!(f, "unknown recovery command 0x{byte:02x}"),
            Self::WriteToReadOnly(command) => write!(f, "{command:?} is read-only"),
            Self::NotServedInState(command) => {
                write!(f, "{command:?} is not served in the device state")
            }
            Self::LengthMismatch { command, got } => {
                write!(f, "{command:?} write carried {got} bytes")
            }
            Self::UnsupportedParameter {
                command,
                offset,
                value,
            } => write!(f, "{command:?} byte {offset} holds 0x{value:02x}"),
        }
    }
}

impl core::error::Error for WireError {}

impl From<WireError> for ProtocolError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::UnknownCommand(_)
            | WireError::WriteToReadOnly(_)
            | WireError::NotServedInState(_) => Self::UnsupportedCommand,
            WireError::LengthMismatch { .. } => Self::LengthWriteError,
            WireError::UnsupportedParameter { .. } => Self::UnsupportedParameter,
        }
    }
}
