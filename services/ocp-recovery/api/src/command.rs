// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The recovery command codes and the access rules that come with them.

use crate::error::WireError;
use crate::regs::Capabilities;
use zerocopy::{Immutable, KnownLayout, TryFromBytes};

/// Whether the Recovery Agent may write a command, or only read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The Recovery Agent reads the command. `check_write` returns
    /// [`WireError::WriteToReadOnly`] for a write.
    ReadOnly,
    /// The Recovery Agent reads and writes the command.
    ReadWrite,
}

/// How many bytes a command carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// The command carries exactly this many bytes.
    Fixed(usize),
    /// The command carries a fixed head followed by a vendor-defined tail, so
    /// the length runs from `min` to `max` inclusive.
    Range {
        /// Smallest accepted length, the fixed head alone.
        min: usize,
        /// Largest accepted length.
        max: usize,
    },
    /// `INDIRECT_DATA`, `INDIRECT_FIFO_DATA` and `VENDOR` carry 1 byte up to
    /// the cap of the transport. For I3C and USB, the caller passes the cap.
    Payload {
        /// The cap section 9.2 of the spec gives for SMBus.
        smbus_max: usize,
    },
}

/// The transport of a transfer, which sets the cap of `INDIRECT_DATA`,
/// `INDIRECT_FIFO_DATA` and `VENDOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// SMBus, where `smbus_max` of [`Length::Payload`] is the cap.
    Smbus,
    /// I3C or USB, where the caller passes the largest payload of 1 access.
    Negotiated(usize),
}

impl Length {
    /// Whether a transfer of `len` bytes fits the command.
    ///
    /// Only [`Length::Payload`] consults `transport`, because the fixed and
    /// ranged lengths come from the spec and hold on every transport.
    pub const fn accepts(self, len: usize, transport: Transport) -> bool {
        match self {
            Self::Fixed(n) => len == n,
            Self::Range { min, max } => min <= len && len <= max,
            Self::Payload { smbus_max } => {
                let max = match transport {
                    Transport::Smbus => smbus_max,
                    Transport::Negotiated(max) => max,
                };
                1 <= len && len <= max
            }
        }
    }
}

/// When the device must answer a command, the scope column of Table 1 in the
/// spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Scope A: the device answers whenever the recovery interface is up.
    Always,
    /// Scope R: the device answers while `DEVICE_STATUS` byte 0 is not 0.
    /// Table 1 marks an active recovery interface by "a non-zero
    /// DEVICE_STATUS", read here as byte 0.
    InterfaceActive,
}

/// Largest length of a command with a fixed head and a vendor-defined tail,
/// and the SMBus cap of `VENDOR`, as section 9.2 of the spec states.
const BLOCK_MAX: usize = 255;

// Section 9.2 gives both caps, and a data aperture payload fits in 1 SMBus
// block.
const _: () = assert!(crate::SMBUS_MAX_DATA_PAYLOAD <= BLOCK_MAX);

/// A length of a fixed head followed by a vendor-defined tail.
const fn head_and_tail(head: usize) -> Length {
    Length::Range {
        min: head,
        max: BLOCK_MAX,
    }
}

/// Access, length, scope and `PROT_CAP` bit of a command.
struct Row {
    access: Access,
    length: Length,
    scope: Scope,
    capability: Option<Capabilities>,
}

impl Row {
    /// Takes the fields in table order, so each row of the table fits 1 line.
    const fn new(
        access: Access,
        length: Length,
        scope: Scope,
        capability: Option<Capabilities>,
    ) -> Self {
        Self {
            access,
            length,
            scope,
            capability,
        }
    }
}

/// A recovery command, named by the command byte of the transaction.
///
/// Decoding the byte once through [`RecoveryCommand::try_from`] gives the
/// handler a value whose access rule, length and scope are settled, so the
/// handler does not re-validate the byte. The spec closes the command set at
/// 0x2f, so the enum is exhaustive and a consumer match covers every command
/// without a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, Immutable, KnownLayout)]
#[repr(u8)]
pub enum RecoveryCommand {
    /// Device capabilities, magic string and protocol version.
    ProtCap = 0x22,
    /// Device identity information.
    DeviceId = 0x23,
    /// Accumulated device status, protocol error and recovery reason code.
    DeviceStatus = 0x24,
    /// Device reset, forced recovery and interface mastering control.
    DeviceReset = 0x25,
    /// Selects the memory region to recover from and activates the image.
    RecoveryCtrl = 0x26,
    /// How far the device has come with the image it is recovering.
    RecoveryStatus = 0x27,
    /// Hardware status, including the composite temperature.
    HwStatus = 0x28,
    /// Selects a memory region for indirect access.
    IndirectCtrl = 0x29,
    /// Status of the region the indirect window selects.
    IndirectStatus = 0x2a,
    /// Read and write aperture of the indirect window.
    IndirectData = 0x2b,
    /// Vendor-defined command.
    Vendor = 0x2c,
    /// Selects a FIFO region and carries the size of the image to come.
    IndirectFifoCtrl = 0x2d,
    /// FIFO indices, region size and maximum transfer size.
    IndirectFifoStatus = 0x2e,
    /// Write aperture the Recovery Agent pushes the image through.
    IndirectFifoData = 0x2f,
}

impl RecoveryCommand {
    /// 1 row per command: the access and the length from section 9.2 of the
    /// spec, the scope from Table 1, and the `PROT_CAP` bit the device checks.
    #[rustfmt::skip]
    const fn row(self) -> Row {
        const FIFO_CMS: Capabilities = Capabilities::FIFO_CMS;
        const HARDWARE_STATUS: Capabilities = Capabilities::HARDWARE_STATUS;
        use Access::{ReadOnly, ReadWrite};
        use Length::Fixed;
        const DATA: Length = Length::Payload { smbus_max: crate::SMBUS_MAX_DATA_PAYLOAD };
        const VENDOR: Length = Length::Payload { smbus_max: BLOCK_MAX };
        // A and R are the letters the scope column of the spec uses.
        use Scope::{Always as A, InterfaceActive as R};
        match self {
            Self::ProtCap            => Row::new(ReadOnly,  Fixed(15),         A, None),
            Self::DeviceId           => Row::new(ReadOnly,  head_and_tail(24), A, None),
            Self::DeviceStatus       => Row::new(ReadOnly,  head_and_tail(7),  A, None),
            Self::DeviceReset        => Row::new(ReadWrite, Fixed(3),          A, None),
            Self::RecoveryCtrl       => Row::new(ReadWrite, Fixed(3),          A, None),
            Self::RecoveryStatus     => Row::new(ReadOnly,  Fixed(2),          A, None),
            Self::HwStatus           => Row::new(ReadOnly,  head_and_tail(4),  R, Some(HARDWARE_STATUS)),
            Self::IndirectCtrl       => Row::new(ReadWrite, Fixed(6),          R, None),
            Self::IndirectStatus     => Row::new(ReadOnly,  Fixed(6),          R, None),
            Self::IndirectData       => Row::new(ReadWrite, DATA,              R, None),
            Self::Vendor             => Row::new(ReadWrite, VENDOR,            R, None),
            Self::IndirectFifoCtrl   => Row::new(ReadWrite, Fixed(6),          R, Some(FIFO_CMS)),
            Self::IndirectFifoStatus => Row::new(ReadOnly,  Fixed(20),         R, Some(FIFO_CMS)),
            Self::IndirectFifoData   => Row::new(ReadWrite, DATA,              R, Some(FIFO_CMS)),
        }
    }

    /// Whether the Recovery Agent may write the command.
    pub const fn access(self) -> Access {
        self.row().access
    }

    /// How many bytes the command carries, as the command table states them.
    pub const fn length(self) -> Length {
        self.row().length
    }

    /// When the device must answer the command.
    pub const fn scope(self) -> Scope {
        self.row().scope
    }

    /// The [`Capabilities`] bit the device checks in `PROT_CAP` before it
    /// serves the command, or `None` where no capability check applies.
    pub const fn required_capability(self) -> Option<Capabilities> {
        self.row().capability
    }

    /// Checks an incoming write against the access rule and the length.
    ///
    /// `check_write` reads no register state, so the device can call it
    /// before it stores a byte of the write.
    pub const fn check_write(self, len: usize, transport: Transport) -> Result<(), WireError> {
        if matches!(self.access(), Access::ReadOnly) {
            return Err(WireError::WriteToReadOnly(self));
        }
        if !self.length().accepts(len, transport) {
            return Err(WireError::LengthMismatch {
                command: self,
                got: len,
            });
        }
        Ok(())
    }
}

// The asserts pin rows of the command table against the spec.
const _: () = {
    assert!(matches!(RecoveryCommand::ProtCap.scope(), Scope::Always));
    assert!(matches!(
        RecoveryCommand::IndirectFifoCtrl.scope(),
        Scope::InterfaceActive
    ));
    assert!(matches!(
        RecoveryCommand::HwStatus.scope(),
        Scope::InterfaceActive
    ));
    assert!(matches!(
        RecoveryCommand::HwStatus.required_capability(),
        Some(c) if c.bits() == Capabilities::HARDWARE_STATUS.bits()
    ));
};

// The length bounds of section 9.2: 7 to 255 bytes for DEVICE_STATUS, and
// on SMBus 1 to 252 for INDIRECT_FIFO_DATA and 1 to 255 for VENDOR.
// Transport::Negotiated replaces the SMBus cap of INDIRECT_FIFO_DATA.
const _: () = {
    // An example cap above SMBUS_MAX_DATA_PAYLOAD, not a spec value.
    const NEGOTIATED_MAX: usize = 1024;

    let device_status = RecoveryCommand::DeviceStatus.length();
    assert!(device_status.accepts(7, Transport::Smbus));
    assert!(device_status.accepts(255, Transport::Smbus));
    assert!(!device_status.accepts(6, Transport::Smbus));
    assert!(!device_status.accepts(256, Transport::Smbus));

    let aperture = RecoveryCommand::IndirectFifoData.length();
    assert!(aperture.accepts(1, Transport::Smbus));
    assert!(aperture.accepts(252, Transport::Smbus));
    assert!(!aperture.accepts(253, Transport::Smbus));
    assert!(!aperture.accepts(0, Transport::Smbus));
    let negotiated = Transport::Negotiated(NEGOTIATED_MAX);
    assert!(aperture.accepts(crate::SMBUS_MAX_DATA_PAYLOAD + 1, negotiated));
    assert!(aperture.accepts(NEGOTIATED_MAX, negotiated));
    assert!(!aperture.accepts(NEGOTIATED_MAX + 1, negotiated));

    let vendor = RecoveryCommand::Vendor.length();
    assert!(!vendor.accepts(0, Transport::Smbus));
    assert!(vendor.accepts(1, Transport::Smbus));
    assert!(vendor.accepts(255, Transport::Smbus));
    assert!(!vendor.accepts(256, Transport::Smbus));
};

// check_write checks the access before the length, so a write of the wrong
// length to a read-only command returns WriteToReadOnly, which
// ProtocolError::from maps to UnsupportedCommand and not to
// LengthWriteError. A write of 0 bytes to INDIRECT_FIFO_DATA returns
// LengthMismatch with the length that arrived, because section 9.2 starts
// the length of INDIRECT_FIFO_DATA at 1 byte.
const _: () = {
    assert!(matches!(
        RecoveryCommand::ProtCap.check_write(3, Transport::Smbus),
        Err(WireError::WriteToReadOnly(RecoveryCommand::ProtCap))
    ));
    assert!(matches!(
        RecoveryCommand::IndirectFifoData.check_write(0, Transport::Smbus),
        Err(WireError::LengthMismatch {
            command: RecoveryCommand::IndirectFifoData,
            got: 0,
        })
    ));
};

impl TryFrom<u8> for RecoveryCommand {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, WireError> {
        Self::try_read_from_bytes(&[value])
            .ok()
            .ok_or(WireError::UnknownCommand(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::ProtocolError;

    #[test]
    fn command_bytes_outside_the_recovery_range_are_refused() {
        assert_eq!(
            RecoveryCommand::try_from(0x21),
            Err(WireError::UnknownCommand(0x21))
        );
        assert_eq!(
            RecoveryCommand::try_from(0x30),
            Err(WireError::UnknownCommand(0x30))
        );
    }

    #[test]
    fn every_command_byte_in_the_recovery_range_decodes() {
        for byte in 0x22..=0x2fu8 {
            let command = RecoveryCommand::try_from(byte).expect("command in range decodes");
            assert_eq!(command as u8, byte);
        }
    }

    #[test]
    fn writing_a_read_only_command_reports_unsupported_command() {
        let err = RecoveryCommand::ProtCap
            .check_write(15, Transport::Smbus)
            .unwrap_err();
        assert_eq!(err, WireError::WriteToReadOnly(RecoveryCommand::ProtCap));
        assert_eq!(ProtocolError::from(err), ProtocolError::UnsupportedCommand);
    }

    #[test]
    fn a_write_of_the_wrong_length_reports_length_write_error() {
        let err = RecoveryCommand::RecoveryCtrl
            .check_write(2, Transport::Smbus)
            .unwrap_err();
        assert_eq!(
            err,
            WireError::LengthMismatch {
                command: RecoveryCommand::RecoveryCtrl,
                got: 2,
            }
        );
        assert_eq!(ProtocolError::from(err), ProtocolError::LengthWriteError);
        assert!(RecoveryCommand::RecoveryCtrl
            .check_write(3, Transport::Smbus)
            .is_ok());
    }
}
