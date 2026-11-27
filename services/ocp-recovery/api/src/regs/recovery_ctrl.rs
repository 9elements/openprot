// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `RECOVERY_CTRL`, which selects the image and activates it.

use core::mem::offset_of;

use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_field, sized_write, struct_matches_command_table};
use crate::command::{Access, RecoveryCommand};
use crate::error::WireError;

/// How the Recovery Agent points the device at an image, in `RECOVERY_CTRL`
/// byte 1.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum ImageSelection {
    /// No operation.
    #[default]
    NoOperation = 0x0,
    /// Use the image in the component memory space that byte 0 selects, which
    /// the agent filled through the memory window or the FIFO.
    FromCms = 0x1,
    /// Use the image the device holds.
    FromDevice = 0x2,
}

/// The values of `RECOVERY_CTRL` byte 2.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum Activate {
    /// Do not activate the recovery image. The device writes this value back
    /// after the activation the agent requested.
    #[default]
    DoNotActivate = 0x0,
    /// The Recovery Agent requests that the device activate the selected
    /// image.
    RecoveryImage = 0xf,
}

/// `RECOVERY_CTRL`, which selects the image and activates it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RecoveryCtrl {
    cms: u8,
    image_selection: ImageSelection,
    activate: Activate,
}

/// Field reads that the device and the Recovery Agent both use.
impl RecoveryCtrl {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The component memory space the agent selected.
    pub fn cms(&self) -> u8 {
        self.cms
    }

    /// Where the agent pointed the device for the image.
    pub fn image_selection(&self) -> ImageSelection {
        self.image_selection
    }

    /// Whether the agent requested an activation the device has not finished.
    pub fn activation_requested(&self) -> bool {
        self.activate == Activate::RecoveryImage
    }
}

/// Operations only the device performs.
impl RecoveryCtrl {
    /// Returns a write off the wire as the array
    /// [`Self::apply_agent_write`] takes, or [`WireError::LengthMismatch`].
    pub fn from_wire(bytes: &[u8]) -> Result<&[u8; Self::SIZE], WireError> {
        sized_write(RecoveryCommand::RecoveryCtrl, bytes)
    }

    /// Takes a write from the Recovery Agent.
    ///
    /// Byte 2 accepts only the [`Activate`] values. A device that treated
    /// byte 2 as a bitwise clear would accept 0x1 here and leave 0xe behind, a
    /// value the spec does not define, so `apply_agent_write` refuses the
    /// write instead.
    pub fn apply_agent_write(&mut self, bytes: &[u8; Self::SIZE]) -> Result<(), WireError> {
        let command = RecoveryCommand::RecoveryCtrl;
        *self = Self {
            cms: bytes[offset_of!(Self, cms)],
            image_selection: decode_field(command, bytes, offset_of!(Self, image_selection))?,
            activate: decode_field(command, bytes, offset_of!(Self, activate))?,
        };
        Ok(())
    }

    /// Zeroes byte 2 after the device activated the image. The agent reads
    /// [`Activate::DoNotActivate`] from then on.
    pub fn clear_activation(&mut self) {
        self.activate = Activate::DoNotActivate;
    }
}

/// Operations only the Recovery Agent performs.
impl RecoveryCtrl {
    /// The bytes a Recovery Agent writes to select an image and, with
    /// [`Activate::RecoveryImage`], to activate it.
    pub fn agent_write(cms: u8, selection: ImageSelection, activate: Activate) -> [u8; Self::SIZE] {
        zerocopy::transmute!(Self {
            cms,
            image_selection: selection,
            activate,
        })
    }
}

const _: () = assert!(struct_matches_command_table::<RecoveryCtrl>(
    RecoveryCommand::RecoveryCtrl
));
const _: () = assert!(matches!(
    RecoveryCommand::RecoveryCtrl.access(),
    Access::ReadWrite
));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::ProtocolError;

    #[test]
    fn from_wire_resolves_the_length_once_for_the_register_that_takes_it() {
        let wire = RecoveryCtrl::agent_write(0, ImageSelection::FromCms, Activate::DoNotActivate);
        let sized = RecoveryCtrl::from_wire(&wire).unwrap();
        assert_eq!(sized, &wire);

        let short = [0u8, ImageSelection::FromCms as u8];
        let err = RecoveryCtrl::from_wire(&short).unwrap_err();
        assert_eq!(ProtocolError::from(err), ProtocolError::LengthWriteError);
    }

    #[test]
    fn writing_one_to_the_activate_field_is_refused_and_changes_nothing() {
        let mut ctrl = RecoveryCtrl::default();
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            0,
            ImageSelection::FromCms,
            Activate::DoNotActivate,
        ))
        .unwrap();

        let err = ctrl
            .apply_agent_write(&[0, ImageSelection::FromCms as u8, 0x1])
            .unwrap_err();
        assert_eq!(
            err,
            WireError::UnsupportedParameter {
                command: RecoveryCommand::RecoveryCtrl,
                offset: 2,
                value: 0x1,
            }
        );
        assert_eq!(
            ProtocolError::from(err),
            ProtocolError::UnsupportedParameter
        );
        assert!(!ctrl.activation_requested());
        assert_eq!(ctrl.as_bytes()[2], Activate::DoNotActivate as u8);
    }

    #[test]
    fn the_agent_activates_and_the_device_clears_the_field() {
        let mut ctrl = RecoveryCtrl::default();
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            2,
            ImageSelection::FromCms,
            Activate::RecoveryImage,
        ))
        .unwrap();
        assert!(ctrl.activation_requested());
        assert_eq!(ctrl.cms(), 2);
        assert_eq!(ctrl.image_selection(), ImageSelection::FromCms);

        ctrl.clear_activation();
        assert!(!ctrl.activation_requested());
        assert_eq!(ctrl.as_bytes()[2], Activate::DoNotActivate as u8);
    }

    #[test]
    fn a_reserved_image_selection_leaves_the_register_as_it_was() {
        let mut ctrl = RecoveryCtrl::default();
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            7,
            ImageSelection::FromDevice,
            Activate::DoNotActivate,
        ))
        .unwrap();

        let err = ctrl
            .apply_agent_write(&[9, 0x3, Activate::RecoveryImage as u8])
            .unwrap_err();
        assert_eq!(
            err,
            WireError::UnsupportedParameter {
                command: RecoveryCommand::RecoveryCtrl,
                offset: 1,
                value: 0x3,
            }
        );
        assert_eq!(ctrl.cms(), 7);
        assert_eq!(ctrl.image_selection(), ImageSelection::FromDevice);
        assert!(!ctrl.activation_requested());
    }
}
