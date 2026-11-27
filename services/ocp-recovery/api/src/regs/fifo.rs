// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! `INDIRECT_FIFO_CTRL` and `INDIRECT_FIFO_STATUS`, the FIFO the Recovery
//! Agent pushes an image through.
//!
//! The registers count sizes and indices in entries of
//! [`FIFO_ENTRY_BYTES`] bytes. The FIFO functions take and return byte
//! counts as `u64`, the smallest type that holds `u32::MAX` entries as a
//! byte count.

use core::mem::offset_of;

use zerocopy::little_endian::{U16, U32};
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

use super::{decode_field, sized_write, struct_matches_command_table};
use crate::command::{Access, RecoveryCommand};
use crate::error::WireError;

/// Bytes in 1 FIFO entry, the "4B unit" of section 9.2.
pub const FIFO_ENTRY_BYTES: u64 = 4;

const fn entries_to_bytes(entries: u32) -> u64 {
    entries as u64 * FIFO_ENTRY_BYTES
}

/// The entry count for `bytes`, or `None` if the count does not fit the
/// 32-bit register field.
const fn bytes_to_entries_rounded_up(bytes: u64) -> Option<u32> {
    let entries = bytes.div_ceil(FIFO_ENTRY_BYTES);
    if entries > u32::MAX as u64 {
        return None;
    }
    Some(entries as u32)
}

/// The values of `INDIRECT_FIFO_CTRL` byte 1.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(u8)]
pub enum FifoReset {
    /// No reset is in flight.
    #[default]
    Idle = 0x0,
    /// The Recovery Agent requests a reset of the FIFO indices.
    Reset = 0x1,
}

/// `INDIRECT_FIFO_CTRL`, which selects the FIFO region and sizes the image.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct IndirectFifoCtrl {
    cms: u8,
    reset: FifoReset,
    image_size_entries: U32,
}

/// Field reads that the device and the Recovery Agent both use.
impl IndirectFifoCtrl {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// The component memory space the Recovery Agent selected.
    pub fn cms(&self) -> u8 {
        self.cms
    }

    /// Whether the Recovery Agent requested a reset the device has not
    /// performed.
    pub fn reset_requested(&self) -> bool {
        self.reset == FifoReset::Reset
    }

    /// Size of the image the Recovery Agent announced, in bytes.
    pub fn image_size_bytes(&self) -> u64 {
        entries_to_bytes(self.image_size_entries.get())
    }
}

/// Operations only the device performs.
impl IndirectFifoCtrl {
    /// Returns a write off the wire as the array
    /// [`Self::apply_agent_write`] takes, or [`WireError::LengthMismatch`].
    pub fn from_wire(bytes: &[u8]) -> Result<&[u8; Self::SIZE], WireError> {
        sized_write(RecoveryCommand::IndirectFifoCtrl, bytes)
    }

    /// Takes a write from the Recovery Agent.
    ///
    /// Byte 1 holds the FIFO reset the agent requests, and the device clears
    /// byte 1 with `clear_reset` when it resets the FIFO. The device starts a
    /// FIFO reset only on this request, because a reset the device started
    /// without one would zero the indices while the agent pushes the next
    /// image.
    pub fn apply_agent_write(&mut self, bytes: &[u8; Self::SIZE]) -> Result<(), WireError> {
        let command = RecoveryCommand::IndirectFifoCtrl;
        let image_size_at = offset_of!(Self, image_size_entries);
        *self = Self {
            cms: bytes[offset_of!(Self, cms)],
            reset: decode_field(command, bytes, offset_of!(Self, reset))?,
            image_size_entries: U32::from_bytes(core::array::from_fn(|i| bytes[image_size_at + i])),
        };
        Ok(())
    }

    /// Zeroes byte 1 after the device reset the FIFO indices.
    pub fn clear_reset(&mut self) {
        self.reset = FifoReset::Idle;
    }
}

/// Operations only the Recovery Agent performs.
impl IndirectFifoCtrl {
    /// The bytes a Recovery Agent writes to select a FIFO region, request a
    /// reset and announce the image size, or `None` for an image of more than
    /// `u32::MAX` entries. The register counts entries, so `agent_write`
    /// rounds a size that does not divide by [`FIFO_ENTRY_BYTES`] up.
    pub fn agent_write(
        cms: u8,
        reset: FifoReset,
        image_size_bytes: u64,
    ) -> Option<[u8; Self::SIZE]> {
        let entries = bytes_to_entries_rounded_up(image_size_bytes)?;
        Some(zerocopy::transmute!(Self {
            cms,
            reset,
            image_size_entries: U32::new(entries),
        }))
    }
}

const _: () = assert!(struct_matches_command_table::<IndirectFifoCtrl>(
    RecoveryCommand::IndirectFifoCtrl
));
const _: () = assert!(matches!(
    RecoveryCommand::IndirectFifoCtrl.access(),
    Access::ReadWrite
));

bitflags::bitflags! {
    /// The bits of `INDIRECT_FIFO_STATUS` byte 0.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FifoStatusBits: u8 {
        /// The FIFO is empty.
        const EMPTY = 1 << 0;
        /// The FIFO is full.
        const FULL = 1 << 1;
    }
}

/// The region types of `INDIRECT_FIFO_STATUS` byte 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(u8)]
pub enum RegionType {
    /// Code space the Recovery Agent writes the recovery image into.
    Code = 0b000,
    /// A log in the debug format, which the Recovery Agent reads.
    Log = 0b001,
    /// A vendor-defined region the Recovery Agent writes.
    VendorWriteOnly = 0b100,
    /// A vendor-defined region the Recovery Agent reads.
    VendorReadOnly = 0b101,
    /// The address space is out of range.
    Unsupported = 0b111,
}

/// `INDIRECT_FIFO_STATUS`, the FIFO indices and sizes.
///
/// The indices count entries. For the write-only code region the Recovery
/// Agent advances the write index and the device advances the read index.
#[repr(C)]
#[derive(Debug, Clone, Copy, TryFromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct IndirectFifoStatus {
    status: u8,
    region_type: RegionType,
    reserved: U16,
    write_index: U32,
    read_index: U32,
    fifo_size_entries: U32,
    max_transfer_entries: U32,
}

/// The write index and the read index reduced modulo the FIFO size, which
/// is above 0. Each field counts entries.
struct ReducedIndices {
    write: u64,
    read: u64,
    size: u64,
}

/// Field reads that the device and the Recovery Agent both use.
impl IndirectFifoStatus {
    /// Encoded size in bytes.
    pub const SIZE: usize = core::mem::size_of::<Self>();

    /// How many entries the FIFO keeps free, so that a full FIFO and an
    /// empty FIFO show different indices (section 8.2.5).
    pub const RESERVED_ENTRIES: u32 = 1;

    /// The bits of byte 0.
    pub fn status(&self) -> FifoStatusBits {
        FifoStatusBits::from_bits_retain(self.status)
    }

    /// The region type.
    pub fn region_type(&self) -> RegionType {
        self.region_type
    }

    /// Largest payload 1 `INDIRECT_FIFO_DATA` access may carry, in bytes.
    pub const fn max_transfer_bytes(&self) -> u64 {
        entries_to_bytes(self.max_transfer_entries.get())
    }

    /// How many bytes the FIFO holds at most: the FIFO size minus
    /// [`Self::RESERVED_ENTRIES`].
    pub const fn usable_bytes(&self) -> u64 {
        entries_to_bytes(
            self.fifo_size_entries
                .get()
                .saturating_sub(Self::RESERVED_ENTRIES),
        )
    }

    /// FIFO Space Remaining of section 8.2.5, in bytes:
    /// (Read Index - (Write Index + 1) + FIFO Size) % FIFO Size entries.
    /// `space_remaining_bytes` returns 0 for a FIFO size of 0.
    pub const fn space_remaining_bytes(&self) -> u64 {
        let Some(ReducedIndices { write, read, size }) = self.reduced_indices() else {
            return 0;
        };
        (read + size - (write + 1) % size) % size * FIFO_ENTRY_BYTES
    }

    /// FIFO Occupancy of section 8.2.5, in bytes:
    /// (Write Index - Read Index + FIFO Size) % FIFO Size entries.
    /// `occupancy_bytes` returns 0 for a FIFO size of 0.
    pub const fn occupancy_bytes(&self) -> u64 {
        let Some(ReducedIndices { write, read, size }) = self.reduced_indices() else {
            return 0;
        };
        (write + size - read) % size * FIFO_ENTRY_BYTES
    }

    /// Both indices reduced modulo the FIFO size, or `None` for a FIFO size
    /// of 0. The reduction keeps each subtraction in the formulas of section
    /// 8.2.5 at 0 or above.
    const fn reduced_indices(&self) -> Option<ReducedIndices> {
        let size = self.fifo_size_entries.get() as u64;
        if size == 0 {
            return None;
        }
        Some(ReducedIndices {
            write: self.write_index.get() as u64 % size,
            read: self.read_index.get() as u64 % size,
            size,
        })
    }
}

/// Operations only the device performs.
impl IndirectFifoStatus {
    /// Describes the code region the Recovery Agent pushes an image into, or
    /// returns `None` for sizes `code_region` refuses. A caller with constant
    /// sizes can unwrap the result in a `const` item, so a refused pair fails
    /// the build.
    ///
    /// The register stores entries, so `code_region` refuses a size that
    /// does not divide by [`FIFO_ENTRY_BYTES`] or does not fit the 32-bit
    /// field. Section 9.2 recommends a FIFO size bigger than the max transfer
    /// size, and `code_region` requires a max transfer size of at most
    /// [`Self::usable_bytes`], because the device NACKs a transfer that does
    /// not fit in the FIFO (section 8.2.5). A max transfer size of 0 lets the
    /// Recovery Agent push no byte, so `code_region` refuses it too.
    ///
    /// In a call `code_region` accepts, the max transfer size is at least
    /// [`FIFO_ENTRY_BYTES`] bytes below the FIFO size. With the 2 arguments
    /// swapped, the max transfer size exceeds the FIFO size, so
    /// `code_region` returns `None`.
    pub const fn code_region(fifo_size_bytes: u64, max_transfer_bytes: u64) -> Option<Self> {
        if fifo_size_bytes % FIFO_ENTRY_BYTES != 0 || max_transfer_bytes % FIFO_ENTRY_BYTES != 0 {
            return None;
        }
        let (Some(fifo_size_entries), Some(max_transfer_entries)) = (
            bytes_to_entries_rounded_up(fifo_size_bytes),
            bytes_to_entries_rounded_up(max_transfer_bytes),
        ) else {
            return None;
        };
        let mut region = Self {
            status: 0,
            region_type: RegionType::Code,
            reserved: U16::ZERO,
            write_index: U32::ZERO,
            read_index: U32::ZERO,
            fifo_size_entries: U32::new(fifo_size_entries),
            max_transfer_entries: U32::new(max_transfer_entries),
        };
        let max_transfer = region.max_transfer_bytes();
        if max_transfer == 0 || max_transfer > region.usable_bytes() {
            return None;
        }
        region.update_status();
        Some(region)
    }

    /// The device calls `reset_indices` after the Recovery Agent requests a
    /// reset in `INDIRECT_FIFO_CTRL` byte 1.
    pub fn reset_indices(&mut self) {
        self.write_index = U32::ZERO;
        self.read_index = U32::ZERO;
        self.update_status();
    }

    /// Records 1 `INDIRECT_FIFO_DATA` write of `bytes` bytes.
    ///
    /// The device drains each payload before it answers, so both indices
    /// advance together and the FIFO reports empty again. The indices count
    /// entries and the device rounds a shorter transfer up to the next entry,
    /// which covers the last chunk of an image whose size does not divide by
    /// [`FIFO_ENTRY_BYTES`]. Both indices wrap at the FIFO size, as the
    /// indices of the Caliptra i3c-core do. A Recovery Agent that counts the
    /// write index modulo the FIFO size reads the same value in
    /// `INDIRECT_FIFO_STATUS`.
    ///
    /// `record_drained_write` takes any `bytes` and reduces the new index
    /// modulo the FIFO size. After a write of exactly the FIFO size, both
    /// indices hold the values they held before. With a FIFO size of 0,
    /// `record_drained_write` sets both indices to 0.
    pub fn record_drained_write(&mut self, bytes: u64) {
        let size = u64::from(self.fifo_size_entries.get());
        let entries = bytes.div_ceil(FIFO_ENTRY_BYTES);
        let next = (u64::from(self.write_index.get()) + entries)
            .checked_rem(size)
            .unwrap_or(0);
        // `next` is 0 or below the FIFO size, so `next` fits a `u32`.
        let next = U32::new(next as u32);
        self.write_index = next;
        self.read_index = next;
        self.update_status();
    }

    /// Sets byte 0 from the indices: `EMPTY` when the FIFO Occupancy is 0
    /// and `FULL` when the FIFO Space Remaining is 0 (section 9.2).
    const fn update_status(&mut self) {
        let mut bits = FifoStatusBits::empty();
        if self.occupancy_bytes() == 0 {
            bits = bits.union(FifoStatusBits::EMPTY);
        }
        if self.space_remaining_bytes() == 0 {
            bits = bits.union(FifoStatusBits::FULL);
        }
        self.status = bits.bits();
    }
}

const _: () = assert!(struct_matches_command_table::<IndirectFifoStatus>(
    RecoveryCommand::IndirectFifoStatus
));
const _: () = assert!(matches!(
    RecoveryCommand::IndirectFifoStatus.access(),
    Access::ReadOnly
));

// The byte numbers of section 9.2 are the truth for the field order.
const _: () = {
    assert!(offset_of!(IndirectFifoCtrl, reset) == 1);
    assert!(offset_of!(IndirectFifoCtrl, image_size_entries) == 2);
    assert!(offset_of!(IndirectFifoStatus, region_type) == 1);
    assert!(offset_of!(IndirectFifoStatus, write_index) == 4);
    assert!(offset_of!(IndirectFifoStatus, read_index) == 8);
    assert!(offset_of!(IndirectFifoStatus, fifo_size_entries) == 12);
    assert!(offset_of!(IndirectFifoStatus, max_transfer_entries) == 16);
};

// The sizes `code_region` refuses and their legal neighbours.
const _: () = {
    let largest = u32::MAX as u64 * 4;
    // A size that does not divide by 4.
    assert!(IndirectFifoStatus::code_region(0x4000, 250).is_none());
    assert!(IndirectFifoStatus::code_region(0x4002, 252).is_none());
    // A FIFO size above `u32::MAX` entries.
    assert!(IndirectFifoStatus::code_region(largest, 252).is_some());
    assert!(IndirectFifoStatus::code_region(largest + 4, 252).is_none());
    // A max transfer size above the usable bytes, which are 1 entry below
    // the FIFO size.
    assert!(IndirectFifoStatus::code_region(0x100, 0x100).is_none());
    assert!(IndirectFifoStatus::code_region(0x100, 0x100 - 4).is_some());
    // Swapped arguments: a max transfer size of 0x4000 in a FIFO of 252
    // bytes.
    assert!(IndirectFifoStatus::code_region(252, 0x4000).is_none());
    // A FIFO that takes no transfer. A FIFO of 1 entry keeps that entry
    // free, and the smallest FIFO has 2 entries.
    assert!(IndirectFifoStatus::code_region(0, 0).is_none());
    assert!(IndirectFifoStatus::code_region(0x100, 0).is_none());
    assert!(IndirectFifoStatus::code_region(4, 0).is_none());
    assert!(IndirectFifoStatus::code_region(4, 4).is_none());
    assert!(IndirectFifoStatus::code_region(8, 4).is_some());
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `INDIRECT_FIFO_STATUS` bytes with the given indices and FIFO size, in
    /// entries.
    fn parsed(write: u32, read: u32, size: u32) -> IndirectFifoStatus {
        let mut bytes = [0u8; IndirectFifoStatus::SIZE];
        bytes[4..8].copy_from_slice(&write.to_le_bytes());
        bytes[8..12].copy_from_slice(&read.to_le_bytes());
        bytes[12..16].copy_from_slice(&size.to_le_bytes());
        IndirectFifoStatus::try_read_from_bytes(&bytes).unwrap()
    }

    /// Bytes 4 to 7 and 8 to 11, the write index and the read index.
    fn indices(region: &IndirectFifoStatus) -> (u32, u32) {
        let bytes = region.as_bytes();
        (
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        )
    }

    #[test]
    fn fifo_ctrl_bytes_match_section_9_2_both_ways() {
        let wire = [0x5a, 0x01, 0x04, 0x03, 0x02, 0x01];
        let mut ctrl = IndirectFifoCtrl::default();
        ctrl.apply_agent_write(IndirectFifoCtrl::from_wire(&wire).unwrap())
            .unwrap();
        assert_eq!(ctrl.cms(), 0x5a);
        assert!(ctrl.reset_requested());
        assert_eq!(ctrl.image_size_bytes(), 0x0102_0304 * 4);
        assert_eq!(ctrl.as_bytes(), wire);
        ctrl.clear_reset();
        assert_eq!(ctrl.as_bytes(), [0x5a, 0x00, 0x04, 0x03, 0x02, 0x01]);

        let written = IndirectFifoCtrl::agent_write(0x5a, FifoReset::Reset, 0x0102_0304 * 4);
        assert_eq!(written, Some(wire));
    }

    #[test]
    fn fifo_ctrl_from_wire_refuses_1_byte_short_and_1_byte_long() {
        for len in [IndirectFifoCtrl::SIZE - 1, IndirectFifoCtrl::SIZE + 1] {
            let wire = [0u8; IndirectFifoCtrl::SIZE + 1];
            assert_eq!(
                IndirectFifoCtrl::from_wire(&wire[..len]),
                Err(WireError::LengthMismatch {
                    command: RecoveryCommand::IndirectFifoCtrl,
                    got: len,
                })
            );
        }
    }

    #[test]
    fn the_fifo_ctrl_rounds_an_image_size_up_to_whole_entries() {
        let mut ctrl = IndirectFifoCtrl::default();
        ctrl.apply_agent_write(&IndirectFifoCtrl::agent_write(0, FifoReset::Idle, 5).unwrap())
            .unwrap();
        assert_eq!(ctrl.image_size_bytes(), 8);
    }

    #[test]
    fn an_image_of_more_than_u32_max_entries_is_refused() {
        let largest = u64::from(u32::MAX) * 4;
        let write = IndirectFifoCtrl::agent_write(0, FifoReset::Idle, largest).unwrap();
        let mut ctrl = IndirectFifoCtrl::default();
        ctrl.apply_agent_write(&write).unwrap();
        assert_eq!(ctrl.image_size_bytes(), largest);
        assert!(IndirectFifoCtrl::agent_write(0, FifoReset::Idle, largest + 1).is_none());
    }

    #[test]
    fn a_refused_fifo_ctrl_write_keeps_the_earlier_fields() {
        let mut ctrl = IndirectFifoCtrl::default();
        let earlier = IndirectFifoCtrl::agent_write(0x5a, FifoReset::Reset, 0x100).unwrap();
        ctrl.apply_agent_write(&earlier).unwrap();

        let mut write = [0u8; IndirectFifoCtrl::SIZE];
        write[1] = 0x2;
        assert_eq!(
            ctrl.apply_agent_write(&write),
            Err(WireError::UnsupportedParameter {
                command: RecoveryCommand::IndirectFifoCtrl,
                offset: 1,
                value: 0x2,
            })
        );
        assert_eq!(ctrl.as_bytes(), earlier);
    }

    #[test]
    fn fifo_ctrl_byte_1_accepts_only_idle_and_reset() {
        let mut ctrl = IndirectFifoCtrl::default();
        let mut write = [0u8; IndirectFifoCtrl::SIZE];
        for value in 0..=u8::MAX {
            write[1] = value;
            let accepted = ctrl.apply_agent_write(&write).is_ok();
            assert_eq!(accepted, value <= 0x1, "byte 1 = {value:#x}");
            if accepted {
                assert_eq!(ctrl.reset_requested(), value == 0x1, "byte 1 = {value:#x}");
            }
        }
    }

    #[test]
    fn fifo_status_bytes_hold_entries() {
        let region = IndirectFifoStatus::code_region(0x100, 0x40).unwrap();
        assert_eq!(
            region.as_bytes(),
            [
                0x01, 0, 0, 0, // EMPTY, code region, reserved
                0, 0, 0, 0, // write index
                0, 0, 0, 0, // read index
                0x40, 0, 0, 0, // FIFO size: 64 entries
                0x10, 0, 0, 0, // max transfer size: 16 entries
            ]
        );
    }

    #[test]
    fn fifo_status_byte_1_gives_the_region_type_of_section_9_2() {
        let defined = [
            (0b000, RegionType::Code),
            (0b001, RegionType::Log),
            (0b100, RegionType::VendorWriteOnly),
            (0b101, RegionType::VendorReadOnly),
            (0b111, RegionType::Unsupported),
        ];
        let mut bytes = [0u8; IndirectFifoStatus::SIZE];
        for value in 0..=u8::MAX {
            bytes[1] = value;
            let parsed = IndirectFifoStatus::try_read_from_bytes(&bytes).ok();
            let expected = defined.iter().find(|(v, _)| *v == value).map(|(_, t)| *t);
            assert_eq!(
                parsed.map(|region| region.region_type()),
                expected,
                "byte 1 = {value:#x}"
            );
        }
    }

    #[test]
    fn fifo_status_keeps_reserved_bits_of_byte_0() {
        let mut bytes = [0u8; IndirectFifoStatus::SIZE];
        bytes[0] = 0xff;
        let region = IndirectFifoStatus::try_read_from_bytes(&bytes).unwrap();
        assert_eq!(region.status().bits(), 0xff);
    }

    #[test]
    fn a_new_code_region_is_empty_code_space() {
        // `SMBUS_MAX_DATA_PAYLOAD` divides by `FIFO_ENTRY_BYTES` and is below
        // the usable bytes of a FIFO of 0x4000 bytes, so `code_region` accepts
        // `SMBUS_MAX_DATA_PAYLOAD` as the max transfer size.
        let max_transfer = crate::SMBUS_MAX_DATA_PAYLOAD as u64;
        let region = IndirectFifoStatus::code_region(0x4000, max_transfer).unwrap();
        assert_eq!(region.status(), FifoStatusBits::EMPTY);
        assert_eq!(region.region_type(), RegionType::Code);
        assert_eq!(region.max_transfer_bytes(), max_transfer);
    }

    #[test]
    fn a_const_item_can_hold_a_code_region() {
        // A refused pair such as (8, 8) makes `SMALLEST` fail the build.
        const SMALLEST: IndirectFifoStatus = IndirectFifoStatus::code_region(8, 4).unwrap();
        assert_eq!(SMALLEST.usable_bytes(), 4);
    }

    #[test]
    fn the_fifo_formulas_follow_section_8_2_5() {
        // Each row: write index, read index, then occupancy and space
        // remaining in entries, for a FIFO of 8 entries (7 usable).
        let rows = [
            (3, 1, 2, 5),  // part full
            (1, 2, 7, 0),  // full: 1 more entry would make write equal read
            (7, 0, 7, 0),  // full without a wrap
            (0, 7, 1, 6),  // the write index wrapped, 1 entry held
            (0, 5, 3, 4),  // the write index wrapped, the read index did not
            (11, 1, 2, 5), // 11 % 8 is 3, the part-full row again
        ];
        for (write, read, occupancy, space) in rows {
            let region = parsed(write, read, 8);
            assert_eq!(region.occupancy_bytes(), occupancy * 4, "{write}, {read}");
            assert_eq!(region.space_remaining_bytes(), space * 4, "{write}, {read}");
            assert_eq!(
                region.occupancy_bytes() + region.space_remaining_bytes(),
                28
            );
        }
    }

    #[test]
    fn the_fifo_formulas_hold_at_the_largest_fifo_size() {
        let region = parsed(u32::MAX - 1, 0, u32::MAX);
        assert_eq!(region.space_remaining_bytes(), 0);
        assert_eq!(region.occupancy_bytes(), region.usable_bytes());
    }

    #[test]
    fn a_drained_fifo_stays_empty() {
        // 64 entries of 4 bytes, 1 of them reserved.
        let mut region = IndirectFifoStatus::code_region(0x100, 0x40).unwrap();
        assert_eq!(region.usable_bytes(), 0x100 - 4);
        assert_eq!(region.occupancy_bytes(), 0);
        assert_eq!(region.space_remaining_bytes(), 0x100 - 4);

        // The device drains each write, so the FIFO stays empty, also after
        // the indices wrap at the FIFO size.
        for _ in 0..100 {
            region.record_drained_write(0x40);
            assert_eq!(region.occupancy_bytes(), 0);
            assert_eq!(region.space_remaining_bytes(), region.usable_bytes());
        }
    }

    #[test]
    fn a_drained_write_wraps_both_indices_at_the_fifo_size() {
        // 4 entries, so the indices run from 0 to 3.
        let mut region = IndirectFifoStatus::code_region(16, 12).unwrap();
        region.record_drained_write(12);
        assert_eq!(indices(&region), (3, 3));
        // 5 bytes round up to 2 entries, and (3 + 2) % 4 is 1.
        region.record_drained_write(5);
        assert_eq!(indices(&region), (1, 1));
        assert_eq!(region.occupancy_bytes(), 0);
        // A write of exactly the FIFO size keeps both indices.
        region.record_drained_write(16);
        assert_eq!(indices(&region), (1, 1));
        // A write of 0 bytes keeps both indices.
        region.record_drained_write(0);
        assert_eq!(indices(&region), (1, 1));
    }

    #[test]
    fn a_drained_write_wraps_at_the_largest_fifo_size() {
        // u32::MAX - 1 + 2 entries is u32::MAX + 1, which is 1 modulo the
        // FIFO size.
        let mut region = parsed(u32::MAX - 1, u32::MAX - 1, u32::MAX);
        region.record_drained_write(8);
        assert_eq!(indices(&region), (1, 1));
    }

    #[test]
    fn a_drained_write_into_a_fifo_of_0_entries_sets_both_indices_to_0() {
        // `code_region` refuses a FIFO size of 0, so the test parses bytes
        // with write index 3 and FIFO size 0.
        let mut region = parsed(3, 0, 0);
        assert_eq!(region.usable_bytes(), 0);
        assert_eq!(region.space_remaining_bytes(), 0);
        assert_eq!(region.occupancy_bytes(), 0);
        region.record_drained_write(4);
        assert_eq!(indices(&region), (0, 0));
    }

    #[test]
    fn a_drained_write_into_a_fifo_of_1_entry_reports_empty_and_full() {
        // `code_region` refuses a FIFO of 1 entry, so the test parses bytes
        // with FIFO size 1.
        let mut region = parsed(0, 0, 1);
        region.record_drained_write(4);
        assert_eq!(
            region.status(),
            FifoStatusBits::EMPTY | FifoStatusBits::FULL
        );
    }

    #[test]
    fn resetting_the_fifo_zeroes_both_indices_and_reports_empty() {
        let mut region = IndirectFifoStatus::code_region(0x4000, 248).unwrap();
        region.record_drained_write(248);
        assert_ne!(indices(&region), (0, 0));
        region.reset_indices();
        assert_eq!(indices(&region), (0, 0));
        assert_eq!(region.status(), FifoStatusBits::EMPTY);
    }
}
