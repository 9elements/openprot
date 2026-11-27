// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The register layouts the device serves to the Recovery Agent.
//!
//! Each register struct matches the byte layout of 1 command, so zerocopy
//! converts between the struct and the bytes of the command. A 1-byte field
//! whose values the spec lists holds its enum, so the field holds only the
//! listed values.
//!
//! `RECOVERY_CTRL` byte 2 and `DEVICE_RESET` byte 0 each hold a field the
//! spec marks "Write 1, Device Clears". The Recovery Agent writes the command
//! value and the device zeroes the field once it has acted, so each of these
//! registers has an `apply_agent_write` that takes the write and a `clear_*`
//! the device calls afterwards.

mod device_reset;
mod device_status;
mod prot_cap;
mod recovery_ctrl;
mod recovery_status;

pub use device_reset::*;
pub use device_status::*;
pub use prot_cap::*;
pub use recovery_ctrl::*;
pub use recovery_status::*;

use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

use crate::command::{Length, RecoveryCommand};
use crate::error::WireError;

bitflags::bitflags! {
    /// The capability bits of `PROT_CAP` bytes 10 and 11.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u16 {
        /// The device serves `DEVICE_ID`.
        const IDENTIFICATION = 1 << 0;
        /// The device enters recovery when the Recovery Agent commands forced
        /// recovery in `DEVICE_RESET`.
        const FORCED_RECOVERY = 1 << 1;
        /// The Recovery Agent may request a management reset in `DEVICE_RESET`.
        const MGMT_RESET = 1 << 2;
        /// The Recovery Agent may request a device reset in `DEVICE_RESET`.
        const DEVICE_RESET = 1 << 3;
        /// The device serves `DEVICE_STATUS`.
        const DEVICE_STATUS = 1 << 4;
        /// The device serves `INDIRECT_CTRL`, `INDIRECT_STATUS` and
        /// `INDIRECT_DATA`.
        const MEMORY_ACCESS = 1 << 5;
        /// The device can use a recovery image it stores (C-image).
        const LOCAL_C_IMAGE = 1 << 6;
        /// The Recovery Agent may push a recovery image to the device.
        const PUSH_C_IMAGE = 1 << 7;
        /// The device disables bus mastering at power-on, so device firmware
        /// cannot block the recovery interface (section 8.3.3).
        const INTERFACE_ISOLATION = 1 << 8;
        /// The device serves `HW_STATUS`.
        const HARDWARE_STATUS = 1 << 9;
        /// The device serves the vendor command.
        const VENDOR_COMMAND = 1 << 10;
        /// The Recovery Agent may select flashless boot mode in `DEVICE_RESET`.
        const FLASHLESS_BOOT = 1 << 11;
        /// The device serves `INDIRECT_FIFO_CTRL`, `INDIRECT_FIFO_STATUS` and
        /// `INDIRECT_FIFO_DATA`.
        const FIFO_CMS = 1 << 12;
    }
}

/// The protocol error codes of `DEVICE_STATUS` byte 1.
///
/// The device latches the code and clears it when the Recovery Agent reads
/// `DEVICE_STATUS`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum ProtocolError {
    /// No protocol error.
    #[default]
    NoError = 0x0,
    /// The command is not supported, or the Recovery Agent wrote a read-only
    /// command.
    UnsupportedCommand = 0x1,
    /// A field held a reserved or undefined value.
    UnsupportedParameter = 0x2,
    /// The write carried the wrong number of bytes.
    LengthWriteError = 0x3,
    /// The PEC of the transaction did not match.
    CrcError = 0x4,
    /// Catch-all for errors the other codes do not classify.
    General = 0xff,
}

/// Whether the size of `T` equals the length the command table gives
/// `command`: the whole length of a fixed command, or the minimum length of
/// a [`Length::Range`] command, the fixed bytes before the vendor data. A
/// [`Length::Payload`] command has no fixed part, so no struct matches. The
/// command table is the truth, and a const assert in each register file
/// checks the struct against the table with `struct_matches_command_table`.
const fn struct_matches_command_table<T>(command: RecoveryCommand) -> bool {
    match command.length() {
        Length::Fixed(len) | Length::Range { min: len, .. } => len == core::mem::size_of::<T>(),
        Length::Payload { .. } => false,
    }
}

// The asserts pin both answers of `struct_matches_command_table`.
const _: () = {
    use RecoveryCommand::{DeviceStatus, ProtCap, Vendor};
    assert!(struct_matches_command_table::<[u8; 15]>(ProtCap));
    assert!(!struct_matches_command_table::<[u8; 14]>(ProtCap));
    assert!(struct_matches_command_table::<[u8; 7]>(DeviceStatus));
    assert!(!struct_matches_command_table::<[u8; 8]>(DeviceStatus));
    assert!(!struct_matches_command_table::<[u8; 1]>(Vendor));
};

/// Decodes 1 byte into a field enum, or `None` if the spec does not define
/// the value.
fn decode_byte<T: TryFromBytes + KnownLayout + Immutable>(value: u8) -> Option<T> {
    const { assert!(core::mem::size_of::<T>() == 1) };
    T::try_read_from_bytes(&[value]).ok()
}

/// Decodes byte `offset` of a write from the Recovery Agent into the field
/// enum at that offset, or reports the offset and the value the spec does
/// not define for the field.
///
/// Every caller passes `offset_of!` of a field of the struct whose bytes
/// `write` holds, so the index stays inside `write`.
fn decode_field<T: TryFromBytes + KnownLayout + Immutable>(
    command: RecoveryCommand,
    write: &[u8],
    offset: usize,
) -> Result<T, WireError> {
    let value = write[offset];
    decode_byte(value).ok_or(WireError::UnsupportedParameter {
        command,
        offset,
        value,
    })
}

/// Returns a write off the wire as the array a fixed-length register takes,
/// or [`WireError::LengthMismatch`] for another length.
fn sized_write<const N: usize>(
    command: RecoveryCommand,
    bytes: &[u8],
) -> Result<&[u8; N], WireError> {
    bytes.try_into().ok().ok_or(WireError::LengthMismatch {
        command,
        got: bytes.len(),
    })
}
