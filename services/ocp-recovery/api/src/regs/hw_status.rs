// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `HW_STATUS`, the hardware status bits and the composite temperature.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use core::mem::offset_of;

use super::struct_matches_command_table;
use crate::command::{Access, RecoveryCommand};

bitflags::bitflags! {
    /// The bits of `HW_STATUS` byte 0.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HwStatusBits: u8 {
        /// The device temperature is critical, and a reset may clear the bit.
        const TEMPERATURE_CRITICAL = 1 << 0;
        /// A hardware soft error, which a reset may clear.
        const SOFT_ERROR = 1 << 1;
        /// A hardware fatal error.
        const FATAL_ERROR = 1 << 2;
    }
}

/// `HW_STATUS` bytes 0 to 3, before the vendor hardware status.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct HwStatus {
    hw_status: u8,
    vendor_hw_status: u8,
    composite_temp: u8,
    vendor_status_len: u8,
}

impl Default for HwStatus {
    /// No status bits, no vendor status and no temperature reading. Byte 2
    /// holds [`HwStatus::TEMPERATURE_UNAVAILABLE`], because 0x00 reads as
    /// 0 degrees Celsius.
    fn default() -> Self {
        Self {
            hw_status: 0,
            vendor_hw_status: 0,
            composite_temp: Self::TEMPERATURE_UNAVAILABLE,
            vendor_status_len: 0,
        }
    }
}

/// Field reads that the device and the Recovery Agent both use.
impl HwStatus {
    /// Encoded size in bytes. The vendor hardware status follows, with the
    /// length byte 3 gives.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// Byte 2 value for no temperature reading, or a reading older than 5
    /// seconds.
    pub const TEMPERATURE_UNAVAILABLE: u8 = 0x80;
    /// Byte 2 value for a failed temperature sensor.
    pub const TEMPERATURE_SENSOR_FAILURE: u8 = 0x81;

    /// The bits of byte 0. Reserved bits stay set, so a Recovery Agent sees
    /// every bit a device sends.
    pub fn status(&self) -> HwStatusBits {
        HwStatusBits::from_bits_retain(self.hw_status)
    }

    /// Byte 2, the composite temperature in degrees Celsius as section 9.2
    /// encodes it: 0x00 to 0x7e hold 0 to 126, 0x7f holds 127 or more, 0xc5
    /// to 0xff hold -59 to -1 as signed bytes, and 0xc4 holds -60 or less.
    /// [`Self::TEMPERATURE_UNAVAILABLE`] and
    /// [`Self::TEMPERATURE_SENSOR_FAILURE`] report no reading, and section 9.2
    /// assigns no temperature to the values 0x82 to 0xc3.
    pub fn composite_temperature(&self) -> u8 {
        self.composite_temp
    }
}

const _: () = assert!(struct_matches_command_table::<HwStatus>(
    RecoveryCommand::HwStatus
));
const _: () = assert!(matches!(
    RecoveryCommand::HwStatus.access(),
    Access::ReadOnly
));

// The byte numbers of section 9.2 are the truth for the field order.
const _: () = {
    assert!(offset_of!(HwStatus, composite_temp) == 2);
    assert!(offset_of!(HwStatus, vendor_status_len) == 3);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_hw_status_reports_no_temperature_reading() {
        assert_eq!(HwStatus::default().as_bytes(), [0, 0, 0x80, 0]);
    }

    #[test]
    fn hw_status_reads_keep_reserved_bits_and_the_temperature_byte() {
        let status = HwStatus::read_from_bytes(&[0xff, 0, 0x81, 0]).unwrap();
        assert_eq!(status.status().bits(), 0xff);
        assert!(status.status().contains(HwStatusBits::FATAL_ERROR));
        assert_eq!(
            status.composite_temperature(),
            HwStatus::TEMPERATURE_SENSOR_FAILURE
        );
    }
}
