// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! [`RecoveryCtrl`] and the types of its fields.

use core::mem::offset_of;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_field, sized_write, struct_matches_command_table};
use crate::command::{Access, RecoveryCommand};
use crate::error::WireError;

/// A component memory space (CMS), which `RECOVERY_CTRL` byte 0 selects. The
/// spec makes 0 the default.
#[repr(transparent)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct ComponentMemorySpace(u8);

impl ComponentMemorySpace {
    /// Takes any `u8`, because the spec allows every index from 0 to 255.
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// The index as byte 0 stores it.
    pub const fn get(self) -> u8 {
        self.0
    }
}

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
    /// the Recovery Agent filled through the memory window or the FIFO.
    FromCms = 0x1,
    /// Use the image the device holds.
    FromDevice = 0x2,
}

/// The values of `RECOVERY_CTRL` byte 2.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum RecoveryImageActivation {
    /// Do not activate the recovery image. The device writes this value back
    /// after the activation the Recovery Agent requested.
    #[default]
    DoNotActivate = 0x0,
    /// The Recovery Agent requests that the device activate the selected
    /// image.
    Activate = 0xf,
}

/// `RECOVERY_CTRL`, in which the Recovery Agent selects an image and requests
/// its activation.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RecoveryCtrl {
    cms: ComponentMemorySpace,
    image_selection: ImageSelection,
    activation: RecoveryImageActivation,
}

/// Field reads that the device and the Recovery Agent both use.
impl RecoveryCtrl {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The component memory space the Recovery Agent selected.
    pub fn cms(&self) -> ComponentMemorySpace {
        self.cms
    }

    /// Where the Recovery Agent pointed the device for the image.
    pub fn image_selection(&self) -> ImageSelection {
        self.image_selection
    }

    /// Byte 2: [`RecoveryImageActivation::Activate`] after the Recovery Agent
    /// requested an activation, until [`Self::clear_activation`] or a later
    /// write replaces it.
    pub fn activation(&self) -> RecoveryImageActivation {
        self.activation
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
    /// Byte 2 accepts only the [`RecoveryImageActivation`] values. A device
    /// that treated byte 2 as a bitwise clear would accept 0x1 here and leave
    /// 0xe behind, a value the spec does not define, so `apply_agent_write`
    /// refuses the write instead. A refused write stores no byte.
    pub fn apply_agent_write(&mut self, bytes: &[u8; Self::SIZE]) -> Result<(), WireError> {
        let command = RecoveryCommand::RecoveryCtrl;
        *self = Self {
            cms: ComponentMemorySpace::new(bytes[offset_of!(Self, cms)]),
            image_selection: decode_field(command, bytes, offset_of!(Self, image_selection))?,
            activation: decode_field(command, bytes, offset_of!(Self, activation))?,
        };
        Ok(())
    }

    /// Zeroes byte 2 after the device activated the image, so the Recovery
    /// Agent reads [`RecoveryImageActivation::DoNotActivate`] until it writes
    /// 0xf again.
    pub fn clear_activation(&mut self) {
        self.activation = RecoveryImageActivation::DoNotActivate;
    }
}

/// Operations only the Recovery Agent performs.
impl RecoveryCtrl {
    /// The bytes a Recovery Agent writes to select an image and, with
    /// [`RecoveryImageActivation::Activate`], to request its activation.
    pub fn agent_write(
        cms: ComponentMemorySpace,
        selection: ImageSelection,
        activation: RecoveryImageActivation,
    ) -> [u8; Self::SIZE] {
        zerocopy::transmute!(Self {
            cms,
            image_selection: selection,
            activation,
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
    fn from_wire_accepts_a_full_write_and_refuses_a_short_one() {
        let wire = RecoveryCtrl::agent_write(
            ComponentMemorySpace::default(),
            ImageSelection::FromCms,
            RecoveryImageActivation::DoNotActivate,
        );
        let sized = RecoveryCtrl::from_wire(&wire).unwrap();
        assert_eq!(sized, &wire);

        let short = [0u8, ImageSelection::FromCms as u8];
        let err = RecoveryCtrl::from_wire(&short).unwrap_err();
        assert_eq!(ProtocolError::from(err), ProtocolError::LengthWriteError);
    }

    #[test]
    fn writing_0x1_to_the_activation_byte_is_refused_and_changes_nothing() {
        let mut ctrl = RecoveryCtrl::default();
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            ComponentMemorySpace::default(),
            ImageSelection::FromCms,
            RecoveryImageActivation::DoNotActivate,
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
        assert_eq!(ctrl.activation(), RecoveryImageActivation::DoNotActivate);
        assert_eq!(
            ctrl.as_bytes()[2],
            RecoveryImageActivation::DoNotActivate as u8
        );
    }

    #[test]
    fn the_recovery_agent_requests_an_activation_and_the_device_clears_it() {
        let mut ctrl = RecoveryCtrl::default();
        let cms = ComponentMemorySpace::new(2);
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            cms,
            ImageSelection::FromCms,
            RecoveryImageActivation::Activate,
        ))
        .unwrap();
        assert_eq!(ctrl.activation(), RecoveryImageActivation::Activate);
        assert_eq!(ctrl.cms(), cms);
        // Byte 0 holds the CMS index.
        assert_eq!(ctrl.as_bytes()[0], 2);
        assert_eq!(ctrl.cms().get(), 2);
        assert_eq!(ctrl.image_selection(), ImageSelection::FromCms);

        ctrl.clear_activation();
        assert_eq!(ctrl.activation(), RecoveryImageActivation::DoNotActivate);
        assert_eq!(
            ctrl.as_bytes()[2],
            RecoveryImageActivation::DoNotActivate as u8
        );
    }

    #[test]
    fn a_reserved_image_selection_is_refused_and_stores_no_byte() {
        let mut ctrl = RecoveryCtrl::default();
        let cms = ComponentMemorySpace::new(7);
        ctrl.apply_agent_write(&RecoveryCtrl::agent_write(
            cms,
            ImageSelection::FromDevice,
            RecoveryImageActivation::DoNotActivate,
        ))
        .unwrap();

        // CMS 9 differs from the stored 7, so a partial store would show.
        let err = ctrl
            .apply_agent_write(&[9, 0x3, RecoveryImageActivation::Activate as u8])
            .unwrap_err();
        assert_eq!(
            err,
            WireError::UnsupportedParameter {
                command: RecoveryCommand::RecoveryCtrl,
                offset: 1,
                value: 0x3,
            }
        );
        assert_eq!(ctrl.cms(), cms);
        assert_eq!(ctrl.image_selection(), ImageSelection::FromDevice);
        assert_eq!(ctrl.activation(), RecoveryImageActivation::DoNotActivate);
    }
}
