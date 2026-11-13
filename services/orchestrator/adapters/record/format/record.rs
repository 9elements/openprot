// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The record the eRoT writes and the boot stage reads.

use crate::crc32::crc32;

/// Bytes one copy of the record takes.
pub const RECORD_LEN: usize = 20;

/// The layout version of the bytes below. A reader refuses a record with
/// another version, so a change to the layout bumps this value.
pub const RECORD_VERSION: u8 = 1;

/// Opens every record copy, so erased or foreign flash does not decode.
const RECORD_MAGIC: [u8; 4] = *b"OPSU";

// Byte offsets inside one copy. Integers are little-endian.
const MAGIC_AT: usize = 0;
const VERSION_AT: usize = 4;
const PHASE_AT: usize = 5;
const CONFIRMED_AT: usize = 6;
const SLOT_AT: usize = 7;
const COUNTER_AT: usize = 8;
const SVN_AT: usize = 12;
const CRC_AT: usize = 16;
const _: () = assert!(CRC_AT + size_of::<u32>() == RECORD_LEN);

// The phase byte. The slot byte holds the trial slot before the confirm and
// the previous slot after it. For a record with no session, `encode` writes
// zero into the slot and SVN bytes, and `decode` ignores them.
const NO_SESSION: u8 = 0;
const PREPARED: u8 = 1;
const TRIAL_PENDING: u8 = 2;
const COMMITTED: u8 = 3;

/// Where a session stands, and the slot the session holds next to the
/// confirmed slot. `decode` refuses a record whose session slot equals the
/// confirmed slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// The eRoT verified the image in `trial` and holds its SVN. The boot
    /// stage still starts the confirmed slot.
    Prepared {
        /// The slot that holds the new image.
        trial: u8,
    },
    /// At the next boot, the boot stage starts `trial`, once.
    TrialPending {
        /// The slot that holds the new image.
        trial: u8,
    },
    /// The trial image ran and the eRoT confirmed it. The confirm wrote the
    /// trial slot in as the confirmed slot, so the boot stage starts the new
    /// image, and a revert writes `previous` back as the confirmed slot.
    Committed {
        /// The confirmed slot before the confirm, which holds the old image.
        previous: u8,
    },
}

impl Phase {
    /// The slot the session holds next to the confirmed slot: the trial
    /// slot, or after the confirm the previous slot.
    #[must_use]
    pub const fn slot(self) -> u8 {
        match self {
            Self::Prepared { trial } | Self::TrialPending { trial } => trial,
            Self::Committed { previous } => previous,
        }
    }
}

/// An update in flight: where the session stands and the SVN of the new
/// image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Session {
    /// Where the session stands, with the slot it holds.
    pub phase: Phase,
    /// The SVN the eRoT read from the verified image.
    pub svn: u32,
}

impl Session {
    /// The slot that holds the new image, before the confirm. `None` after
    /// the confirm, which made the trial slot the confirmed slot.
    #[must_use]
    pub const fn trial(self) -> Option<u8> {
        match self.phase {
            Phase::Prepared { trial } | Phase::TrialPending { trial } => Some(trial),
            Phase::Committed { .. } => None,
        }
    }
}

/// One record: which slot is confirmed, and the session in flight if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    /// Counts writes. The copy with the higher counter, compared with
    /// wrap-around, holds the newest record.
    pub counter: u32,
    /// The slot that holds the confirmed image.
    pub confirmed: u8,
    /// The update in flight. `None` when the eRoT has no update open.
    pub session: Option<Session>,
}

/// Why a copy did not decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// The copy does not open with the record magic: erased or foreign
    /// flash.
    Magic,
    /// The copy carries another layout version.
    Version(u8),
    /// The checksum does not match: a torn write or a flash fault.
    Checksum,
    /// The phase byte names no phase.
    Phase(u8),
    /// The session slot equals the confirmed slot.
    SlotIsConfirmed,
}

impl Record {
    /// The record after this one: the counter one up, the given contents.
    #[must_use]
    pub const fn next(&self, confirmed: u8, session: Option<Session>) -> Self {
        Self {
            counter: self.counter.wrapping_add(1),
            confirmed,
            session,
        }
    }

    /// Whether the eRoT wrote `self` after `other`. Compares the counters
    /// with wrap-around, so the order holds across a counter overflow as long
    /// as the counters of copies A and B differ by less than `i32::MAX`. The
    /// writer alternates the copies, so the counters differ by 1.
    #[must_use]
    pub const fn is_newer_than(&self, other: &Self) -> bool {
        (self.counter.wrapping_sub(other.counter) as i32) > 0
    }

    /// Whether `known` accepts every slot the record names: the confirmed
    /// slot, and the slot the session holds.
    #[must_use]
    pub fn names_known_slots(&self, mut known: impl FnMut(u8) -> bool) -> bool {
        known(self.confirmed)
            && self
                .session
                .is_none_or(|session| known(session.phase.slot()))
    }

    /// Whether the record steers the boot stage to `slot`: the confirmed
    /// slot, or the trial slot of a `TrialPending` session. The boot stage
    /// starts the trial slot of a `Prepared` session or the previous slot of
    /// a `Committed` session only when it walks.
    #[must_use]
    pub fn steers_to(&self, slot: u8) -> bool {
        self.confirmed == slot
            || self.session.is_some_and(
                |session| matches!(session.phase, Phase::TrialPending { trial } if trial == slot),
            )
    }

    /// The bytes of one copy: integers little-endian, the checksum last.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut bytes = [0; RECORD_LEN];
        bytes[MAGIC_AT..VERSION_AT].copy_from_slice(&RECORD_MAGIC);
        bytes[VERSION_AT] = RECORD_VERSION;
        bytes[CONFIRMED_AT] = self.confirmed;
        bytes[COUNTER_AT..SVN_AT].copy_from_slice(&self.counter.to_le_bytes());
        if let Some(session) = self.session {
            bytes[PHASE_AT] = match session.phase {
                Phase::Prepared { .. } => PREPARED,
                Phase::TrialPending { .. } => TRIAL_PENDING,
                Phase::Committed { .. } => COMMITTED,
            };
            bytes[SLOT_AT] = session.phase.slot();
            bytes[SVN_AT..CRC_AT].copy_from_slice(&session.svn.to_le_bytes());
        } else {
            bytes[PHASE_AT] = NO_SESSION;
        }
        let crc = crc32(&bytes[..CRC_AT]);
        bytes[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// Reads one copy. Checks the magic, the version and the checksum
    /// before any field, so a torn or erased copy reads as an error.
    pub fn decode(bytes: &[u8; RECORD_LEN]) -> Result<Self, DecodeError> {
        if bytes[MAGIC_AT..VERSION_AT] != RECORD_MAGIC {
            return Err(DecodeError::Magic);
        }
        if bytes[VERSION_AT] != RECORD_VERSION {
            return Err(DecodeError::Version(bytes[VERSION_AT]));
        }
        if crc32(&bytes[..CRC_AT]) != le_u32(bytes, CRC_AT) {
            return Err(DecodeError::Checksum);
        }
        let confirmed = bytes[CONFIRMED_AT];
        let slot = bytes[SLOT_AT];
        let phase = match bytes[PHASE_AT] {
            NO_SESSION => None,
            PREPARED => Some(Phase::Prepared { trial: slot }),
            TRIAL_PENDING => Some(Phase::TrialPending { trial: slot }),
            COMMITTED => Some(Phase::Committed { previous: slot }),
            other => return Err(DecodeError::Phase(other)),
        };
        if phase.is_some() && slot == confirmed {
            return Err(DecodeError::SlotIsConfirmed);
        }
        let session = phase.map(|phase| Session {
            phase,
            svn: le_u32(bytes, SVN_AT),
        });
        Ok(Self {
            counter: le_u32(bytes, COUNTER_AT),
            confirmed,
            session,
        })
    }
}

/// The little-endian `u32` at `at`.
fn le_u32(bytes: &[u8; RECORD_LEN], at: usize) -> u32 {
    let mut word = [0; size_of::<u32>()];
    word.copy_from_slice(&bytes[at..at + size_of::<u32>()]);
    u32::from_le_bytes(word)
}

/// Copy A or copy B of the record, on storage that cannot write a record
/// atomically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CopyId {
    /// The first copy.
    A,
    /// The second copy.
    B,
}

impl CopyId {
    /// The copy the next write goes to when `self` holds the newest record.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// The newest valid record of copies A and B, and the copy that holds it.
///
/// A writer always writes the copy that does not hold the newest record, so
/// a torn write damages only the copy this function then skips. `None` when
/// neither copy decodes.
#[must_use]
pub fn newest(a: Option<Record>, b: Option<Record>) -> Option<(CopyId, Record)> {
    match (a, b) {
        (Some(a), Some(b)) if b.is_newer_than(&a) => Some((CopyId::B, b)),
        (Some(a), _) => Some((CopyId::A, a)),
        (None, Some(b)) => Some((CopyId::B, b)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter of the record under test.
    const COUNTER: u32 = 41;

    const PENDING: Record = Record {
        counter: COUNTER,
        confirmed: 0,
        session: Some(Session {
            phase: Phase::TrialPending { trial: 1 },
            svn: 7,
        }),
    };

    /// `PENDING` after the confirm: slot 1 confirmed, slot 0 previous.
    const COMMITTED: Record = Record {
        counter: COUNTER + 1,
        confirmed: 1,
        session: Some(Session {
            phase: Phase::Committed { previous: 0 },
            svn: 7,
        }),
    };

    #[test]
    fn a_record_reads_back_as_written() {
        let idle = Record {
            counter: 3,
            confirmed: 1,
            session: None,
        };
        for record in [idle, PENDING, COMMITTED] {
            assert_eq!(Record::decode(&record.encode()), Ok(record));
        }
    }

    #[test]
    fn erased_flash_does_not_decode() {
        assert_eq!(Record::decode(&[0xFF; RECORD_LEN]), Err(DecodeError::Magic));
        assert_eq!(Record::decode(&[0; RECORD_LEN]), Err(DecodeError::Magic));
    }

    #[test]
    fn a_flipped_bit_fails_the_checksum() {
        let mut bytes = PENDING.encode();
        bytes[SVN_AT] ^= 1;
        assert_eq!(Record::decode(&bytes), Err(DecodeError::Checksum));
    }

    // A torn write leaves a prefix of the new copy on erased flash. No
    // prefix short of the whole copy decodes.
    #[test]
    fn no_torn_prefix_decodes() {
        let full = PENDING.encode();
        for cut in 0..RECORD_LEN {
            let mut bytes = [0xFF; RECORD_LEN];
            bytes[..cut].copy_from_slice(&full[..cut]);
            assert!(Record::decode(&bytes).is_err(), "prefix of {cut} bytes");
        }
    }

    #[test]
    fn another_layout_version_is_refused() {
        let mut bytes = PENDING.encode();
        bytes[VERSION_AT] = RECORD_VERSION + 1;
        assert_eq!(
            Record::decode(&bytes),
            Err(DecodeError::Version(RECORD_VERSION + 1))
        );
    }

    #[test]
    fn a_session_slot_on_the_confirmed_slot_is_refused() {
        let pending = Record {
            confirmed: 1,
            ..PENDING
        };
        let committed = Record {
            confirmed: 0,
            ..COMMITTED
        };
        for record in [pending, committed] {
            assert_eq!(
                Record::decode(&record.encode()),
                Err(DecodeError::SlotIsConfirmed)
            );
        }
    }

    #[test]
    fn newest_picks_the_higher_counter_across_wrap_around() {
        let old = Record {
            counter: u32::MAX,
            ..PENDING
        };
        let new = old.next(0, None);
        assert_eq!(new.counter, 0);
        assert_eq!(newest(Some(old), Some(new)), Some((CopyId::B, new)));
        assert_eq!(newest(Some(new), Some(old)), Some((CopyId::A, new)));
    }

    #[test]
    fn the_record_steers_to_the_session_slot_only_while_the_trial_is_pending() {
        assert!(PENDING.steers_to(0));
        assert!(PENDING.steers_to(1));
        let prepared = Record {
            session: Some(Session {
                phase: Phase::Prepared { trial: 1 },
                svn: 7,
            }),
            ..PENDING
        };
        assert!(prepared.steers_to(0));
        assert!(!prepared.steers_to(1));
        assert!(COMMITTED.steers_to(1));
        assert!(!COMMITTED.steers_to(0));
    }

    #[test]
    fn newest_reads_the_one_valid_copy() {
        assert_eq!(newest(None, Some(PENDING)), Some((CopyId::B, PENDING)));
        assert_eq!(newest(Some(PENDING), None), Some((CopyId::A, PENDING)));
        assert_eq!(newest(None, None), None);
    }
}
