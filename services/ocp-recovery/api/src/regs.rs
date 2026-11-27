// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The register layouts the device serves to the Recovery Agent.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
