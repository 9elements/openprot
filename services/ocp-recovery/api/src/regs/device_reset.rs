// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `DEVICE_RESET`, the reset and forced-recovery control.

use core::mem::offset_of;

use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_field, sized_write, struct_matches_command_table};
use crate::command::{Access, RecoveryCommand};
use crate::error::WireError;

/// The reset the Recovery Agent commands in `DEVICE_RESET` byte 0.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum ResetControl {
    /// No reset.
    #[default]
    NoReset = 0x0,
    /// Reset the device, which disrupts the bus.
    ResetDevice = 0x1,
    /// Reset the management subsystem without disrupting the bus.
    ResetManagement = 0x2,
}

/// The mode the Recovery Agent selects in `DEVICE_RESET` byte 1, which the
/// device enters at the next platform reset.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum ForcedRecovery {
    /// No forced recovery: the device boots normally.
    #[default]
    NoForcedRecovery = 0x0,
    /// Boot with no local image and stream one over the recovery interface.
    FlashlessBoot = 0xe,
    /// Enter recovery mode.
    RecoveryMode = 0xf,
}

/// The values of `DEVICE_RESET` byte 2.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum InterfaceMastering {
    /// Interface mastering is off, the state at power-on.
    #[default]
    Disabled = 0x0,
    /// The Recovery Agent enables interface mastering.
    Enabled = 0x1,
}

/// `DEVICE_RESET`, the reset and forced-recovery control.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct DeviceReset {
    reset_control: ResetControl,
    forced_recovery: ForcedRecovery,
    interface_control: InterfaceMastering,
}

/// Field reads that the device and the Recovery Agent both use.
impl DeviceReset {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The reset the agent commanded.
    pub fn reset_control(&self) -> ResetControl {
        self.reset_control
    }

    /// The mode the agent selected for the next platform reset.
    pub fn forced_recovery(&self) -> ForcedRecovery {
        self.forced_recovery
    }

    /// Whether the agent enabled interface mastering.
    pub fn interface_mastering(&self) -> InterfaceMastering {
        self.interface_control
    }
}

/// Operations only the device performs.
impl DeviceReset {
    /// Returns a write off the wire as the array
    /// [`Self::apply_agent_write`] takes, or [`WireError::LengthMismatch`].
    pub fn from_wire(bytes: &[u8]) -> Result<&[u8; Self::SIZE], WireError> {
        sized_write(RecoveryCommand::DeviceReset, bytes)
    }

    /// Takes a write from the Recovery Agent.
    ///
    /// `apply_agent_write` decodes every byte before it stores any, so a
    /// refused write stores no byte.
    pub fn apply_agent_write(&mut self, bytes: &[u8; Self::SIZE]) -> Result<(), WireError> {
        let command = RecoveryCommand::DeviceReset;
        *self = Self {
            reset_control: decode_field(command, bytes, offset_of!(Self, reset_control))?,
            forced_recovery: decode_field(command, bytes, offset_of!(Self, forced_recovery))?,
            interface_control: decode_field(command, bytes, offset_of!(Self, interface_control))?,
        };
        Ok(())
    }

    /// Zeroes byte 0 after the device performed the reset the agent
    /// requested. The forced-recovery and interface bytes keep their values:
    /// the spec has the device clear only the reset control.
    pub fn clear_reset_control(&mut self) {
        self.reset_control = ResetControl::NoReset;
    }
}

/// Operations only the Recovery Agent performs.
impl DeviceReset {
    /// The bytes a Recovery Agent writes to request a reset, a forced
    /// recovery mode and the interface mastering state.
    pub fn agent_write(
        reset: ResetControl,
        forced: ForcedRecovery,
        mastering: InterfaceMastering,
    ) -> [u8; Self::SIZE] {
        zerocopy::transmute!(Self {
            reset_control: reset,
            forced_recovery: forced,
            interface_control: mastering,
        })
    }
}

const _: () = assert!(struct_matches_command_table::<DeviceReset>(
    RecoveryCommand::DeviceReset
));
const _: () = assert!(matches!(
    RecoveryCommand::DeviceReset.access(),
    Access::ReadWrite
));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_reset_builds_the_example_write_of_the_spec() {
        // Section 9.2: management reset and forced recovery mode.
        assert_eq!(
            DeviceReset::agent_write(
                ResetControl::ResetManagement,
                ForcedRecovery::RecoveryMode,
                InterfaceMastering::Disabled,
            ),
            [0x02, 0x0f, 0x00]
        );
    }

    #[test]
    fn device_reset_clears_only_the_reset_control_byte() {
        let mut reset = DeviceReset::default();
        reset
            .apply_agent_write(&DeviceReset::agent_write(
                ResetControl::ResetManagement,
                ForcedRecovery::RecoveryMode,
                InterfaceMastering::Enabled,
            ))
            .unwrap();
        assert_eq!(reset.reset_control(), ResetControl::ResetManagement);
        assert_eq!(reset.forced_recovery(), ForcedRecovery::RecoveryMode);
        assert_eq!(reset.interface_mastering(), InterfaceMastering::Enabled);

        reset.clear_reset_control();
        assert_eq!(reset.reset_control(), ResetControl::NoReset);
        assert_eq!(reset.forced_recovery(), ForcedRecovery::RecoveryMode);
        assert_eq!(reset.interface_mastering(), InterfaceMastering::Enabled);
    }

    #[test]
    fn a_reserved_forced_recovery_value_is_refused() {
        let mut reset = DeviceReset::default();
        let err = reset
            .apply_agent_write(&[
                ResetControl::NoReset as u8,
                0x5,
                InterfaceMastering::Disabled as u8,
            ])
            .unwrap_err();
        assert_eq!(
            err,
            WireError::UnsupportedParameter {
                command: RecoveryCommand::DeviceReset,
                offset: 1,
                value: 0x5,
            }
        );
        assert_eq!(reset.forced_recovery(), ForcedRecovery::NoForcedRecovery);
    }

    #[test]
    fn an_undefined_interface_mastering_value_is_refused() {
        let mut reset = DeviceReset::default();
        let err = reset
            .apply_agent_write(&[
                ResetControl::NoReset as u8,
                ForcedRecovery::NoForcedRecovery as u8,
                0x2,
            ])
            .unwrap_err();
        assert_eq!(
            err,
            WireError::UnsupportedParameter {
                command: RecoveryCommand::DeviceReset,
                offset: 2,
                value: 0x2,
            }
        );
        assert_eq!(reset.as_bytes(), &[0, 0, 0]);
    }
}
