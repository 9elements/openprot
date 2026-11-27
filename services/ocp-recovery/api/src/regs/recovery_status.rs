// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `RECOVERY_STATUS`, which reports the recovery state and the image it
//! refers to.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_byte, struct_matches_command_table};
use crate::command::RecoveryCommand;

/// The recovery states of `RECOVERY_STATUS` byte 0, bits 0 to 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, Immutable, KnownLayout)]
#[repr(u8)]
pub enum RecoveryState {
    /// The device is not in recovery mode.
    NotInRecovery = 0x0,
    /// The device waits for the agent to push the image.
    AwaitingImage = 0x1,
    /// The device is booting the image it received.
    BootingImage = 0x2,
    /// The image booted.
    Successful = 0x3,
    /// Recovery failed for a reason the other error states do not name.
    Failed = 0xc,
    /// The image failed authentication.
    AuthenticationError = 0xd,
    /// The device could not enter recovery mode, which an administrator may
    /// have disabled.
    EnterRecoveryError = 0xe,
    /// The agent named a component address space the device does not hold.
    InvalidComponentAddressSpace = 0xf,
}

/// Which image a [`RecoveryStatus`] refers to, bounded to the 4-bit field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageIndex(u8);

impl ImageIndex {
    /// Largest index the field holds.
    pub const MAX: u8 = u8::MAX >> RecoveryStatus::INDEX_SHIFT;

    /// Names an image, or `None` if the index does not fit the field.
    pub const fn new(index: u8) -> Option<Self> {
        if index > Self::MAX {
            return None;
        }
        Some(Self(index))
    }

    /// The index as the field stores it.
    pub const fn get(self) -> u8 {
        self.0
    }
}

// The spec gives the image index 4 bits.
const _: () = assert!(ImageIndex::MAX == 0xf);

/// `RECOVERY_STATUS`, which reports the recovery state and the image it
/// refers to.
///
/// The register is read-only to the agent. Bits 4 to 7 hold the index of the
/// image the state refers to, so a failure names the image that failed. The
/// state and the index share byte 0, so the struct stores the raw byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RecoveryStatus {
    status: u8,
    vendor_status: u8,
}

/// Field reads that the device and the Recovery Agent both use.
impl RecoveryStatus {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// Bits 0 to 3 hold the state and bits 4 to 7 the image index.
    const INDEX_SHIFT: u8 = 4;

    const STATE_MASK: u8 = (1 << Self::INDEX_SHIFT) - 1;

    /// The reported state, or `None` if the bits hold a value the spec does
    /// not define (0x4 to 0xb).
    pub fn state(&self) -> Option<RecoveryState> {
        decode_byte(self.status & Self::STATE_MASK)
    }

    /// Which image the reported state refers to.
    pub fn image(&self) -> ImageIndex {
        ImageIndex(self.status >> Self::INDEX_SHIFT)
    }
}

/// Operations only the device performs.
impl RecoveryStatus {
    /// Reports a state for one image.
    ///
    /// [`ImageIndex`] bounds the index to the 4 bits the field holds, so no
    /// index truncates into the state bits and the call cannot fail.
    pub fn set(&mut self, state: RecoveryState, image: ImageIndex) {
        self.status = (image.get() << Self::INDEX_SHIFT) | (state as u8);
    }
}

const _: () = assert!(struct_matches_command_table::<RecoveryStatus>(
    RecoveryCommand::RecoveryStatus
));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_status_packs_the_image_index_above_the_state() {
        let mut status = RecoveryStatus::default();
        let image = ImageIndex::new(2).unwrap();
        status.set(RecoveryState::AuthenticationError, image);
        assert_eq!(status.state(), Some(RecoveryState::AuthenticationError));
        assert_eq!(status.image(), image);
        assert_eq!(status.as_bytes()[0], 0x2d);
    }

    #[test]
    fn an_image_index_wider_than_the_field_has_no_value() {
        assert_eq!(ImageIndex::new(ImageIndex::MAX + 1), None);
        assert_eq!(
            ImageIndex::new(ImageIndex::MAX).map(ImageIndex::get),
            Some(ImageIndex::MAX)
        );
    }

    #[test]
    fn the_largest_image_index_does_not_reach_the_state_bits() {
        let mut status = RecoveryStatus::default();
        let last = ImageIndex::new(ImageIndex::MAX).unwrap();
        status.set(RecoveryState::Failed, last);
        assert_eq!(status.state(), Some(RecoveryState::Failed));
        assert_eq!(status.image(), last);
    }
}
