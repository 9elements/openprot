// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `PROT_CAP`, the capabilities the device advertises.

use core::mem::offset_of;

use zerocopy::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{struct_matches_command_table, Capabilities};
use crate::command::RecoveryCommand;

/// The magic string that opens `PROT_CAP`, "OCP RECV" in ASCII.
pub const PROT_CAP_MAGIC: [u8; 8] = *b"OCP RECV";

const PROT_CAP_VERSION_MAJOR: u8 = 1;

const PROT_CAP_VERSION_MINOR: u8 = 1;

/// `PROT_CAP` byte 13: the device answers each operation within
/// 2^`exponent` microseconds.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct MaxResponseTime {
    /// The exponent, as byte 13 holds it.
    pub exponent: u8,
}

/// `PROT_CAP` byte 14: a heartbeat period of 2^`exponent` microseconds.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct HeartbeatPeriod {
    /// The exponent, as byte 14 holds it.
    pub exponent: u8,
}

impl HeartbeatPeriod {
    /// The device does not support the heartbeat.
    pub const NOT_SUPPORTED: Self = Self { exponent: 0 };
}

/// `PROT_CAP` bytes 10 and 11, the [`Capabilities`] bits in the little-endian
/// order of the wire.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct CapabilityBits(U16);

impl CapabilityBits {
    const fn new(capabilities: Capabilities) -> Self {
        Self(U16::new(capabilities.bits()))
    }

    /// `from_bits_retain` keeps the bits the spec marks "Reserved" and
    /// gives no meaning, so a Recovery Agent sees every bit a device sends.
    fn get(self) -> Capabilities {
        Capabilities::from_bits_retain(self.0.get())
    }
}

/// `PROT_CAP`, the capabilities the device advertises.
///
/// The device fills `PROT_CAP` at init, because the Recovery Agent reads it
/// whenever the recovery interface is up.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct ProtCap {
    magic: [u8; 8],
    version_major: u8,
    version_minor: u8,
    capabilities: CapabilityBits,
    cms_count: u8,
    max_response_time: MaxResponseTime,
    heartbeat_period: HeartbeatPeriod,
}

/// Field reads that the device and the Recovery Agent both use.
impl ProtCap {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The advertised capability bits, the reserved bits included.
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities.get()
    }

    /// Whether the device advertises every bit in `bits`.
    pub fn advertises(&self, bits: Capabilities) -> bool {
        self.capabilities().contains(bits)
    }
}

/// Operations only the device performs.
impl ProtCap {
    /// Builds `PROT_CAP` for a device.
    pub const fn new(
        capabilities: Capabilities,
        cms_count: u8,
        max_response_time: MaxResponseTime,
        heartbeat_period: HeartbeatPeriod,
    ) -> Self {
        Self {
            magic: PROT_CAP_MAGIC,
            version_major: PROT_CAP_VERSION_MAJOR,
            version_minor: PROT_CAP_VERSION_MINOR,
            capabilities: CapabilityBits::new(capabilities),
            cms_count,
            max_response_time,
            heartbeat_period,
        }
    }
}

/// Operations only the Recovery Agent performs.
impl ProtCap {
    /// Reads `PROT_CAP` as a Recovery Agent receives it, or `None` if the
    /// bytes have the wrong length, do not open with [`PROT_CAP_MAGIC`] or
    /// carry another major version. `parse` does not check byte 9, so it also
    /// returns `PROT_CAP` of a device that reports another minor version.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let cap = Self::read_from_bytes(bytes).ok()?;
        (cap.magic == PROT_CAP_MAGIC && cap.version_major == PROT_CAP_VERSION_MAJOR).then_some(cap)
    }
}

const _: () = assert!(struct_matches_command_table::<ProtCap>(
    RecoveryCommand::ProtCap
));

// The byte numbers of section 9.2 are the truth for the field order.
const _: () = {
    assert!(offset_of!(ProtCap, version_major) == 8);
    assert!(offset_of!(ProtCap, version_minor) == 9);
    assert!(offset_of!(ProtCap, capabilities) == 10);
    assert!(offset_of!(ProtCap, cms_count) == 12);
    assert!(offset_of!(ProtCap, max_response_time) == 13);
    assert!(offset_of!(ProtCap, heartbeat_period) == 14);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prot_cap_new_writes_the_magic_string_the_version_and_the_exponents() {
        let response_time = MaxResponseTime { exponent: 10 };
        let heartbeat = HeartbeatPeriod { exponent: 20 };
        let cap = ProtCap::new(
            Capabilities::IDENTIFICATION | Capabilities::PUSH_C_IMAGE | Capabilities::FIFO_CMS,
            3,
            response_time,
            heartbeat,
        );
        let bytes = cap.as_bytes();
        assert_eq!(bytes.len(), ProtCap::SIZE);
        assert_eq!(&bytes[0..8], &PROT_CAP_MAGIC);
        assert_eq!(bytes[8], PROT_CAP_VERSION_MAJOR);
        assert_eq!(bytes[9], PROT_CAP_VERSION_MINOR);
        // Bits 0, 7 and 12 in little-endian order, then the CMS count.
        assert_eq!(bytes[10..13], [0x81, 0x10, 0x03]);
        assert_eq!(bytes[13], response_time.exponent);
        assert_eq!(bytes[14], heartbeat.exponent);
        assert!(cap.advertises(Capabilities::PUSH_C_IMAGE | Capabilities::FIFO_CMS));
        assert!(!cap.advertises(Capabilities::HARDWARE_STATUS));
        assert!(!cap.advertises(Capabilities::PUSH_C_IMAGE | Capabilities::HARDWARE_STATUS));
    }

    #[test]
    fn prot_cap_parse_checks_the_length_the_magic_string_and_the_major_version() {
        let cap = ProtCap::new(
            Capabilities::PUSH_C_IMAGE,
            1,
            MaxResponseTime { exponent: 0 },
            HeartbeatPeriod::NOT_SUPPORTED,
        );
        let parsed = ProtCap::parse(cap.as_bytes()).unwrap();
        assert!(parsed.advertises(Capabilities::PUSH_C_IMAGE));

        let mut wrong_magic = [0u8; ProtCap::SIZE];
        wrong_magic.copy_from_slice(cap.as_bytes());
        wrong_magic[0] = b'X';
        assert!(ProtCap::parse(&wrong_magic).is_none());

        let mut next_major = [0u8; ProtCap::SIZE];
        next_major.copy_from_slice(cap.as_bytes());
        next_major[8] = PROT_CAP_VERSION_MAJOR + 1;
        assert!(ProtCap::parse(&next_major).is_none());

        let mut next_minor = [0u8; ProtCap::SIZE];
        next_minor.copy_from_slice(cap.as_bytes());
        next_minor[9] = PROT_CAP_VERSION_MINOR + 1;
        assert!(ProtCap::parse(&next_minor).is_some());

        assert!(ProtCap::parse(&cap.as_bytes()[1..]).is_none());
        let mut long = [0u8; ProtCap::SIZE + 1];
        long[..ProtCap::SIZE].copy_from_slice(cap.as_bytes());
        assert!(ProtCap::parse(&long).is_none());
    }

    #[test]
    fn prot_cap_keeps_a_reserved_capability_bit() {
        // Bits 13 to 15 are reserved in the spec.
        const RESERVED_BIT_13: u16 = 1 << 13;
        let cap = ProtCap::new(
            Capabilities::empty(),
            1,
            MaxResponseTime { exponent: 0 },
            HeartbeatPeriod::NOT_SUPPORTED,
        );
        let mut bytes = [0u8; ProtCap::SIZE];
        bytes.copy_from_slice(cap.as_bytes());
        bytes[10..12].copy_from_slice(&RESERVED_BIT_13.to_le_bytes());
        let parsed = ProtCap::parse(&bytes).unwrap();
        assert_eq!(parsed.capabilities().bits(), RESERVED_BIT_13);
    }
}
