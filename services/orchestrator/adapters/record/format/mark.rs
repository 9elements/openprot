// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! What the boot stage writes for itself and what it hands to the image.

use crate::crc32::crc32;

/// Bytes the tried mark takes.
pub const MARK_LEN: usize = 12;

/// Opens the tried mark, so an erased region reads as no mark.
const MARK_MAGIC: [u8; 4] = *b"OPTM";

const MAGIC_AT: usize = 0;
const COUNTER_AT: usize = 4;
const CRC_AT: usize = 8;
const _: () = assert!(CRC_AT + size_of::<u32>() == MARK_LEN);

/// The counter of the record whose trial the boot stage started.
///
/// Before the jump into the trial slot, the boot stage writes the mark into
/// a region that only the boot stage writes, reads the mark back and locks
/// the region until reset. A mark that holds the counter of the newest
/// record means the trial already ran, so the boot stage starts the
/// confirmed slot and each record write gets one trial boot. The eRoT does
/// not write or read the region: a new session is a new record write, and
/// its counter differs from the mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TriedMark {
    /// The counter of the record whose trial the boot stage started.
    pub counter: u32,
}

impl TriedMark {
    /// The bytes of the mark: the counter little-endian, the checksum last.
    #[must_use]
    pub fn encode(&self) -> [u8; MARK_LEN] {
        let mut bytes = [0; MARK_LEN];
        bytes[MAGIC_AT..COUNTER_AT].copy_from_slice(&MARK_MAGIC);
        bytes[COUNTER_AT..CRC_AT].copy_from_slice(&self.counter.to_le_bytes());
        let crc = crc32(&bytes[..CRC_AT]);
        bytes[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// Reads the mark. `None` for an erased region, a torn write or any
    /// other bytes that fail the magic or the checksum.
    #[must_use]
    pub fn decode(bytes: &[u8; MARK_LEN]) -> Option<Self> {
        if bytes[MAGIC_AT..COUNTER_AT] != MARK_MAGIC {
            return None;
        }
        let word = |at: usize| {
            let mut word = [0; size_of::<u32>()];
            word.copy_from_slice(&bytes[at..at + size_of::<u32>()]);
            u32::from_le_bytes(word)
        };
        if crc32(&bytes[..CRC_AT]) != word(CRC_AT) {
            return None;
        }
        Some(Self {
            counter: word(COUNTER_AT),
        })
    }
}

/// What the boot stage tells the image it started.
///
/// The boot stage checked the signature of the image before the jump and
/// read the SVN from its signed header, so both values come from a checked
/// image. The platform picks the mechanism that hands the values to the
/// image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handover {
    /// The slot the boot stage started.
    pub started: u8,
    /// The SVN from the signed header of the started image.
    pub svn: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_reads_back_as_written() {
        let mark = TriedMark { counter: 41 };
        assert_eq!(TriedMark::decode(&mark.encode()), Some(mark));
    }

    #[test]
    fn an_erased_region_holds_no_mark() {
        assert_eq!(TriedMark::decode(&[0xFF; MARK_LEN]), None);
        assert_eq!(TriedMark::decode(&[0; MARK_LEN]), None);
    }

    #[test]
    fn a_torn_mark_reads_as_no_mark() {
        let full = TriedMark { counter: 41 }.encode();
        for cut in 0..MARK_LEN {
            let mut bytes = [0xFF; MARK_LEN];
            bytes[..cut].copy_from_slice(&full[..cut]);
            assert_eq!(TriedMark::decode(&bytes), None, "prefix of {cut} bytes");
        }
    }
}
