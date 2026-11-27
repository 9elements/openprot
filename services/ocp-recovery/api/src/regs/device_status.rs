// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `DEVICE_STATUS`, the device state, the protocol error and the recovery
//! reason code.

use core::mem::offset_of;

use zerocopy::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_byte, struct_matches_command_table, ProtocolError};
use crate::command::RecoveryCommand;

/// The device states of `DEVICE_STATUS` byte 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(u8)]
pub enum DeviceState {
    /// The device has not determined its state yet.
    StatusPending = 0x0,
    /// The device is running its operational firmware.
    Healthy = 0x1,
    /// The device reports a soft error or another error state.
    DeviceError = 0x2,
    /// The device is in recovery mode and will accept an image.
    RecoveryModeReady = 0x3,
    /// The device holds an image and waits for the Recovery Agent to activate
    /// it.
    RecoveryPending = 0x4,
    /// The device is running the image it recovered.
    RunningRecoveryImage = 0x5,
    /// The boot halted, and the reason code says why.
    BootFailure = 0xe,
    /// The device reports a fatal error.
    FatalError = 0xf,
}

impl DeviceState {
    /// Whether the device serves the commands of [`Scope::InterfaceActive`]
    /// in this state. Every state but `StatusPending` marks an active
    /// recovery interface.
    ///
    /// [`Scope::InterfaceActive`]: crate::command::Scope::InterfaceActive
    pub const fn interface_active(self) -> bool {
        // The match names every state and has no `_` arm, so a patch that adds
        // a state does not build until the match names the state.
        match self {
            Self::StatusPending => false,
            Self::Healthy
            | Self::DeviceError
            | Self::RecoveryModeReady
            | Self::RecoveryPending
            | Self::RunningRecoveryImage
            | Self::BootFailure
            | Self::FatalError => true,
        }
    }
}

/// The recovery reason codes of `DEVICE_STATUS` bytes 2 and 3.
///
/// The names spell out the abbreviations of Table 3 (Recovery Reason Codes),
/// which each variant doc gives in parentheses. Vendor codes 0x80 to 0xff
/// are legal and `RecoveryReason` does not name them, see
/// [`ReasonCode::vendor`].
///
/// Every code `RecoveryReason` names fits in byte 2, and byte 3 is 0, so
/// `RecoveryReason` is 1 byte and has no byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, Immutable, KnownLayout)]
#[repr(u8)]
pub enum RecoveryReason {
    /// No boot failure detected (BFNF).
    NoFailure = 0x0,
    /// Generic hardware error (BFGHWE).
    GenericHardwareError = 0x1,
    /// Generic hardware soft error, which may be recoverable (BFGSE).
    GenericSoftError = 0x2,
    /// A self-test failed (BFSTF).
    SelfTestFailure = 0x3,
    /// Critical data is missing or corrupt (BFCD).
    CorruptedCriticalData = 0x4,
    /// The key manifest is missing or corrupt (BFKMMC).
    KeyManifestMissing = 0x5,
    /// The key manifest failed authentication (BFKMAF).
    KeyManifestAuthFailure = 0x6,
    /// The key manifest failed the anti-rollback check (BFKIAR, as Table 3
    /// prints it).
    KeyManifestRollback = 0x7,
    /// The boot loader image is missing or corrupt (BFFIMC).
    BootLoaderMissing = 0x8,
    /// The boot loader image failed authentication (BFFIAF).
    BootLoaderAuthFailure = 0x9,
    /// The boot loader image failed the anti-rollback check (BFFIAR).
    BootLoaderRollback = 0xa,
    /// The main firmware image is missing or corrupt (BFMFMC).
    MainFirmwareMissing = 0xb,
    /// The main firmware image failed authentication (BFMFAF).
    MainFirmwareAuthFailure = 0xc,
    /// The main firmware image failed the anti-rollback check (BFMFAR).
    MainFirmwareRollback = 0xd,
    /// The recovery firmware is missing or corrupt (BFRFMC).
    RecoveryFirmwareMissing = 0xe,
    /// The recovery firmware failed authentication (BFRFAF).
    RecoveryFirmwareAuthFailure = 0xf,
    /// The recovery firmware failed the anti-rollback check (BFRFAR).
    RecoveryFirmwareRollback = 0x10,
    /// The Recovery Agent commanded forced recovery (FR).
    ForcedRecovery = 0x11,
    /// The device boots with no local image and streams one over the
    /// recovery interface (FSB).
    FlashlessStreamingBoot = 0x12,
}

/// `DEVICE_STATUS` bytes 2 and 3, the recovery reason code.
///
/// [`ReasonCode::spec`] and [`ReasonCode::vendor`] build only the codes
/// [`RecoveryReason`] names and vendor codes. A code parsed from a device can
/// hold any value, which [`ReasonCode::decode`] returns as `Spec`, `Vendor` or
/// `Reserved`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct ReasonCode(U16);

/// A [`ReasonCode`] classified as a code [`RecoveryReason`] names, a vendor
/// code or a reserved code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedReason {
    /// A code [`RecoveryReason`] names.
    Spec(RecoveryReason),
    /// A vendor code, 0x80 to 0xff.
    Vendor(u8),
    /// A code Table 3 marks "Reserved" and gives no meaning: 0x13 to 0x7f
    /// and every code above 0xff.
    Reserved(u16),
}

impl ReasonCode {
    /// The first vendor code of Table 3. The vendor codes run to 0xff.
    pub const VENDOR_FIRST: u8 = 0x80;

    /// Code 0, which a state without a reason code reports.
    pub const NO_FAILURE: Self = Self::spec(RecoveryReason::NoFailure);

    /// Every `RecoveryReason` variant is a code Table 3 defines, so `spec`
    /// returns `Self` where [`ReasonCode::vendor`] returns an `Option`.
    pub const fn spec(reason: RecoveryReason) -> Self {
        Self(U16::new(reason as u16))
    }

    /// A vendor code, or `None` if `code` is below [`Self::VENDOR_FIRST`].
    pub const fn vendor(code: u8) -> Option<Self> {
        if code < Self::VENDOR_FIRST {
            return None;
        }
        Some(Self(U16::new(code as u16)))
    }

    /// A device can write any 16-bit value to bytes 2 and 3, so `decode`
    /// returns `Reserved` for a code Table 3 marks "Reserved".
    pub fn decode(self) -> DecodedReason {
        let raw = self.0.get();
        let Ok(code) = u8::try_from(raw) else {
            return DecodedReason::Reserved(raw);
        };
        if let Some(reason) = decode_byte::<RecoveryReason>(code) {
            return DecodedReason::Spec(reason);
        }
        if code >= Self::VENDOR_FIRST {
            return DecodedReason::Vendor(code);
        }
        DecodedReason::Reserved(raw)
    }
}

/// The heartbeat counter of `DEVICE_STATUS` bytes 4 and 5.
///
/// Section 9.2 gives the heartbeat a range of 0 to 4095, and
/// [`Heartbeat::next`] wraps from 4095 to 0. A Recovery Agent can still parse
/// a larger value from a device that breaks the range.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Heartbeat(U16);

impl Heartbeat {
    /// The counter value at init, before the first tick.
    pub const ZERO: Self = Self(U16::ZERO);

    /// Largest value section 9.2 allows.
    pub const MAX: Self = Self(U16::new(4095));

    /// The counter in the byte order of the CPU.
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    /// The value after 1 tick, 0 after [`Self::MAX`].
    pub const fn next(self) -> Self {
        if self.get() >= Self::MAX.get() {
            return Self::ZERO;
        }
        Self(U16::new(self.get() + 1))
    }
}

/// A [`DeviceState`] the device reports, with the reason code for the states
/// that section 9.2 and Table 2 (Recovery Reason Code Population) give
/// one: 0x3, 0x4 and 0xe.
///
/// The other states hold no reason code, so a caller cannot pair a reason
/// code with `Healthy`. [`DeviceStatus::set_state`] writes
/// [`ReasonCode::NO_FAILURE`] for a state without a reason code, which
/// replaces the code of an earlier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    missing_docs,
    reason = "`DeviceState` documents each state under the same variant name"
)]
pub enum ReportedState {
    StatusPending,
    Healthy,
    DeviceError,
    RecoveryModeReady(ReasonCode),
    RecoveryPending(ReasonCode),
    RunningRecoveryImage,
    BootFailure(ReasonCode),
    FatalError,
}

impl ReportedState {
    /// The state as byte 0 holds it.
    pub const fn state(self) -> DeviceState {
        use DeviceState as D;
        match self {
            Self::StatusPending => D::StatusPending,
            Self::Healthy => D::Healthy,
            Self::DeviceError => D::DeviceError,
            Self::RecoveryModeReady(_) => D::RecoveryModeReady,
            Self::RecoveryPending(_) => D::RecoveryPending,
            Self::RunningRecoveryImage => D::RunningRecoveryImage,
            Self::BootFailure(_) => D::BootFailure,
            Self::FatalError => D::FatalError,
        }
    }

    const fn reason_code(self) -> ReasonCode {
        match self {
            Self::RecoveryModeReady(code)
            | Self::RecoveryPending(code)
            | Self::BootFailure(code) => code,
            Self::StatusPending
            | Self::Healthy
            | Self::DeviceError
            | Self::RunningRecoveryImage
            | Self::FatalError => ReasonCode::NO_FAILURE,
        }
    }
}

/// `DEVICE_STATUS` bytes 0 to 6, before the vendor status.
///
/// Byte 1 holds the protocol error, which
/// [`DeviceStatus::take_protocol_error`] reads and clears.
#[repr(C)]
#[derive(Debug, Clone, Copy, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct DeviceStatus {
    state: DeviceState,
    protocol_error: ProtocolError,
    reason_code: ReasonCode,
    heartbeat: Heartbeat,
    vendor_status_len: u8,
}

/// Field reads that the device and the Recovery Agent both use.
impl DeviceStatus {
    /// Encoded size in bytes. The vendor status follows, with the length
    /// byte 6 gives.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The state, with its reason code for a state that carries one. A
    /// caller classifies the code with [`ReasonCode::decode`]. For a state
    /// without a reason code, `reported` ignores bytes 2 and 3.
    pub fn reported(&self) -> ReportedState {
        use DeviceState as D;
        use ReportedState as R;
        let code = self.reason_code;
        // The match names every state and has no `_` arm, so a patch that
        // adds a state does not build until ReportedState covers it.
        match self.state {
            D::StatusPending => R::StatusPending,
            D::Healthy => R::Healthy,
            D::DeviceError => R::DeviceError,
            D::RecoveryModeReady => R::RecoveryModeReady(code),
            D::RecoveryPending => R::RecoveryPending(code),
            D::RunningRecoveryImage => R::RunningRecoveryImage,
            D::BootFailure => R::BootFailure(code),
            D::FatalError => R::FatalError,
        }
    }

    /// The heartbeat counter.
    pub fn heartbeat(&self) -> Heartbeat {
        self.heartbeat
    }
}

/// Operations only the device performs.
impl DeviceStatus {
    /// `DEVICE_STATUS` as the device reports it at init, before the first
    /// state change.
    pub const fn pending() -> Self {
        Self {
            state: DeviceState::StatusPending,
            protocol_error: ProtocolError::NoError,
            reason_code: ReasonCode::NO_FAILURE,
            heartbeat: Heartbeat::ZERO,
            vendor_status_len: 0,
        }
    }

    /// Reports a state and its reason code, see [`ReportedState`].
    pub fn set_state(&mut self, reported: ReportedState) {
        self.state = reported.state();
        self.reason_code = reported.reason_code();
    }

    /// Latches a protocol error for the Recovery Agent to read.
    ///
    /// A second error before the Recovery Agent reads overwrites the first,
    /// because the spec defines a single error byte.
    pub fn set_protocol_error(&mut self, error: ProtocolError) {
        self.protocol_error = error;
    }

    /// Reads the protocol error and clears it, the clear-on-read the spec
    /// defines for byte 1.
    pub fn take_protocol_error(&mut self) -> ProtocolError {
        core::mem::take(&mut self.protocol_error)
    }

    /// Increments the heartbeat counter, which wraps from 4095 to 0.
    pub fn tick_heartbeat(&mut self) {
        self.heartbeat = self.heartbeat.next();
    }
}

const _: () = assert!(struct_matches_command_table::<DeviceStatus>(
    RecoveryCommand::DeviceStatus
));

// The byte numbers of section 9.2 are the truth for the field order.
const _: () = {
    assert!(offset_of!(DeviceStatus, protocol_error) == 1);
    assert!(offset_of!(DeviceStatus, reason_code) == 2);
    assert!(offset_of!(DeviceStatus, heartbeat) == 4);
    assert!(offset_of!(DeviceStatus, vendor_status_len) == 6);
};

// The asserts pin the Table 2 states that carry a reason code, and the
// `interface_active` result for StatusPending and Healthy.
const _: () = {
    let code = ReasonCode::NO_FAILURE;
    assert!(ReportedState::RecoveryModeReady(code).state() as u8 == 0x3);
    assert!(ReportedState::RecoveryPending(code).state() as u8 == 0x4);
    assert!(ReportedState::BootFailure(code).state() as u8 == 0xe);
    assert!(!DeviceState::StatusPending.interface_active());
    assert!(DeviceState::Healthy.interface_active());
};

// Section 9.2 gives the heartbeat a range of 0 to 4095.
const _: () = {
    assert!(Heartbeat(U16::new(4094)).next().get() == 4095);
    assert!(Heartbeat(U16::new(4095)).next().get() == 0);
    // A value above 4095 can only come from a device that breaks the range.
    // `next` returns 0 for it, so 1 tick brings the counter back into range.
    assert!(Heartbeat(U16::new(4096)).next().get() == 0);
};

// Table 3 starts the vendor codes at 0x80.
const _: () = {
    assert!(ReasonCode::VENDOR_FIRST == 0x80);
    assert!(ReasonCode::vendor(0x7f).is_none());
    assert!(ReasonCode::vendor(0x80).is_some());
    assert!(ReasonCode::vendor(0xff).is_some());
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_the_protocol_error_clears_it() {
        let mut status = DeviceStatus::pending();
        status.set_protocol_error(ProtocolError::LengthWriteError);
        assert_eq!(
            status.take_protocol_error(),
            ProtocolError::LengthWriteError
        );
        assert_eq!(status.take_protocol_error(), ProtocolError::NoError);
        assert_eq!(status.as_bytes()[1], 0x0);
    }

    #[test]
    fn a_state_that_carries_no_reason_code_reports_zero() {
        let mut status = DeviceStatus::pending();
        let code = ReasonCode::spec(RecoveryReason::MainFirmwareAuthFailure);
        status.set_state(ReportedState::BootFailure(code));
        assert_eq!(status.reported(), ReportedState::BootFailure(code));
        assert_eq!(status.as_bytes()[2..4], [0x0c, 0x00]);

        // Healthy carries no reason code, so set_state writes 0.
        status.set_state(ReportedState::Healthy);
        assert_eq!(status.as_bytes()[2..4], [0x00, 0x00]);
    }

    #[test]
    fn set_state_and_reported_agree_on_every_state() {
        let code = ReasonCode::vendor(0x80).unwrap();
        // Section 9.2: each state and its byte 0 value.
        let cases = [
            (ReportedState::StatusPending, 0x0),
            (ReportedState::Healthy, 0x1),
            (ReportedState::DeviceError, 0x2),
            (ReportedState::RecoveryModeReady(code), 0x3),
            (ReportedState::RecoveryPending(code), 0x4),
            (ReportedState::RunningRecoveryImage, 0x5),
            (ReportedState::BootFailure(code), 0xe),
            (ReportedState::FatalError, 0xf),
        ];
        for (reported, byte) in cases {
            let mut status = DeviceStatus::pending();
            status.set_state(reported);
            assert_eq!(status.as_bytes()[0], byte);
            assert_eq!(status.reported(), reported);
        }
    }

    #[test]
    fn reported_returns_no_reason_code_for_a_state_that_carries_none() {
        let mut bytes = [0u8; DeviceStatus::SIZE];
        bytes[0] = DeviceState::Healthy as u8;
        // Reason code 0x5 (KeyManifestMissing), which Healthy does not carry.
        bytes[2] = 0x05;
        let status = DeviceStatus::try_read_from_bytes(&bytes).unwrap();
        assert_eq!(status.reported(), ReportedState::Healthy);
    }

    #[test]
    fn tick_heartbeat_stores_the_counter_low_byte_first() {
        // 0x102 ticks give a counter whose 2 bytes differ, so a swap shows.
        const TICKS: u16 = 0x102;
        let mut status = DeviceStatus::pending();
        for _ in 0..TICKS {
            status.tick_heartbeat();
        }
        assert_eq!(status.heartbeat().get(), TICKS);
        assert_eq!(status.as_bytes()[4..6], [0x02, 0x01]);
    }

    #[test]
    fn only_the_spec_protocol_errors_parse() {
        // Section 9.2: the protocol errors of byte 1.
        const PROTOCOL_ERRORS: [u8; 6] = [0x0, 0x1, 0x2, 0x3, 0x4, 0xff];
        for value in 0..=u8::MAX {
            let mut bytes = [0u8; DeviceStatus::SIZE];
            bytes[1] = value;
            assert_eq!(
                DeviceStatus::try_read_from_bytes(&bytes).is_ok(),
                PROTOCOL_ERRORS.contains(&value),
                "protocol error {value:#x}"
            );
        }
    }

    #[test]
    fn a_reserved_device_state_does_not_parse() {
        let mut bytes = [0u8; DeviceStatus::SIZE];
        // Device state 0x6, which section 9.2 marks "Reserved" and gives no
        // meaning.
        bytes[0] = 0x6;
        assert!(DeviceStatus::try_read_from_bytes(&bytes).is_err());
    }

    #[test]
    fn set_state_stores_a_vendor_reason_code_low_byte_first() {
        let mut status = DeviceStatus::pending();
        let code = ReasonCode::vendor(0xa5).unwrap();
        status.set_state(ReportedState::BootFailure(code));
        assert_eq!(status.as_bytes()[2..4], [0xa5, 0x00]);
        assert_eq!(status.reported(), ReportedState::BootFailure(code));
        assert_eq!(code.decode(), DecodedReason::Vendor(0xa5));
    }

    #[test]
    fn a_reserved_reason_code_parses() {
        let mut bytes = [0u8; DeviceStatus::SIZE];
        bytes[0] = DeviceState::BootFailure as u8;
        // Reason code 0x0100, which Table 3 marks "Reserved" and gives no
        // meaning.
        bytes[3] = 0x01;
        let status = DeviceStatus::try_read_from_bytes(&bytes).unwrap();
        assert_eq!(
            status.reported(),
            ReportedState::BootFailure(ReasonCode(U16::new(0x100)))
        );
    }

    #[test]
    fn every_table_3_code_decodes_to_the_variant_with_that_number() {
        const LAST_TABLE_3_CODE: u16 = 0x12;
        for raw in 0..=LAST_TABLE_3_CODE {
            let decoded = ReasonCode(U16::new(raw)).decode();
            assert!(
                matches!(decoded, DecodedReason::Spec(reason) if reason as u16 == raw),
                "{raw:#x} decodes to {decoded:?}"
            );
        }
    }

    #[test]
    fn reason_code_decodes_the_table_3_boundaries() {
        let decode = |raw| ReasonCode(U16::new(raw)).decode();
        assert_eq!(
            decode(0x12),
            DecodedReason::Spec(RecoveryReason::FlashlessStreamingBoot)
        );
        assert_eq!(decode(0x13), DecodedReason::Reserved(0x13));
        assert_eq!(decode(0x7f), DecodedReason::Reserved(0x7f));
        assert_eq!(decode(0x80), DecodedReason::Vendor(0x80));
        assert_eq!(decode(0xff), DecodedReason::Vendor(0xff));
        assert_eq!(decode(0x100), DecodedReason::Reserved(0x100));
    }
}
