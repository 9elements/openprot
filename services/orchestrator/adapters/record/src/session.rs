// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! [`SelfUpdate`] over record copies A and B on [`Storage`].

use orchestrator_capabilities::{BootSelection, RunningImage, SelfUpdate, SelfUpdateState, Svn};
use orchestrator_config::{ImageLayout, SlotId};
use orchestrator_self_update_format::{
    newest, pick_trial_slot, CopyId, Handover, Phase, Record, Session, RECORD_LEN,
};
use util_io::Storage;

/// Where record copies A and B sit on the storage.
///
/// [`Storage`] erases on the first write into an erase block and hides the
/// erase from the caller, so copies A and B must sit in different erase
/// blocks: a write into one copy then cannot wipe the other.
#[derive(Clone, Copy, Debug)]
pub struct RecordCopies {
    a: usize,
    b: usize,
}

impl RecordCopies {
    /// Places copy A at offset `a` and copy B at offset `b`. Const, so a bad
    /// board table fails the build.
    ///
    /// # Panics
    ///
    /// Panics when a copy does not fit one erase block, when an offset does
    /// not start an erase block, or when both copies share an erase block.
    #[must_use]
    pub const fn new(a: usize, b: usize, erase_block: usize) -> Self {
        assert!(
            RECORD_LEN <= erase_block,
            "a record copy must fit one erase block"
        );
        assert!(
            a % erase_block == 0 && b % erase_block == 0,
            "each record copy must start an erase block"
        );
        assert!(
            a != b,
            "the record copies must sit in different erase blocks"
        );
        Self { a, b }
    }

    const fn offset(&self, copy: CopyId) -> usize {
        match copy {
            CopyId::A => self.a,
            CopyId::B => self.b,
        }
    }
}

/// What [`RecordSession::open`] found on the storage, for the board to log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// Both copies decode.
    Intact,
    /// One copy decodes and the other does not: a torn write, a flash fault,
    /// or a new part after the first record write, which fills copy A. The
    /// next write goes to the copy that does not decode.
    OneValidCopy,
    /// The eRoT wrote a fresh record, with the started slot as the confirmed
    /// slot and no session.
    Rewritten(Rewrite),
}

/// Why [`RecordSession::open`] wrote a fresh record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rewrite {
    /// Neither copy decodes: a new part, both copies damaged, or a record
    /// of another layout version, which `open` overwrites. The boot stage
    /// walked the slots.
    NoValidCopy,
    /// The newest record names a slot outside the layout.
    UnknownSlot,
    /// The boot stage started a slot the record does not steer to: the slots
    /// the record steers to failed their checks and the boot stage walked.
    /// The walk can start the trial slot of a `Prepared` session or the
    /// previous slot of a `Committed` session, which then becomes the
    /// confirmed slot. The fresh record holds no session.
    Walked,
}

/// Why a [`RecordSession`] call failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError<E> {
    /// The storage failed. The record in RAM keeps the value it had, so a
    /// retry writes the same copy. The flash can already hold the new
    /// record, when the write landed and only the completion failed:
    /// `state` then reports the old state until the next boot reads the
    /// flash.
    Storage(E),
    /// The called transition starts from a state other than the state the
    /// record holds, for example `set_trial_pending` while the record holds
    /// `Idle`. The variant carries the state the record holds.
    WrongState(SelfUpdateState),
    /// The boot stage started the confirmed slot in this boot, and the image
    /// in the trial slot did not run, so `confirm` refuses to write the trial
    /// slot in as the confirmed slot.
    TrialNotRunning,
    /// Every slot is the confirmed slot, the slot running now or the previous
    /// slot of a `Committed` session, so an update has no slot to write its
    /// image into.
    NoFreeSlot,
    /// The handover names a slot outside the layout.
    UnknownStartedSlot(SlotId),
}

impl<E> From<E> for RecordError<E> {
    fn from(err: E) -> Self {
        Self::Storage(err)
    }
}

impl<E: core::fmt::Debug> core::fmt::Display for RecordError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Storage(err) => write!(f, "self-update record storage: {err:?}"),
            Self::WrongState(state) => {
                write!(f, "self-update transition not allowed from {state:?}")
            }
            Self::NoFreeSlot => f.write_str("no slot is free for a self-update"),
            Self::TrialNotRunning => {
                f.write_str("the boot stage started the confirmed slot, so confirm has no trial")
            }
            Self::UnknownStartedSlot(slot) => {
                write!(
                    f,
                    "the boot stage started slot {} outside the layout",
                    slot.0
                )
            }
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for RecordError<E> {}

/// The self-update session of the eRoT, kept in record copies A and B.
///
/// [`open`](Self::open) reads both copies once per boot and keeps the
/// newest record in RAM, so [`state`](SelfUpdate::state) and
/// [`running`](SelfUpdate::running) read no storage. Each transition writes
/// the copy that does not hold the newest record, with the counter one up,
/// in a separate storage session, and waits for the write to finish. A
/// transition the record already reflects writes nothing.
pub struct RecordSession<S> {
    storage: S,
    copies: RecordCopies,
    layout: ImageLayout,
    handover: Handover,
    /// What [`running`](SelfUpdate::running) answers, fixed at
    /// [`open`](Self::open) so a confirm, a revert or a complete in this boot
    /// does not change the answer.
    selected: BootSelection,
    record: Record,
    newest: CopyId,
}

impl<S: Storage> RecordSession<S> {
    /// Reads the record and reconciles it with the slot the boot stage
    /// started.
    ///
    /// Writes a fresh record, with the started slot confirmed and no
    /// session, when no copy decodes, when the record names a slot outside
    /// `layout`, or when the record does not steer the boot stage to the
    /// started slot, so the boot stage walked. [`Found`] says which
    /// happened.
    ///
    /// # Errors
    ///
    /// [`RecordError::UnknownStartedSlot`] when `handover` names a slot
    /// outside `layout`, and [`RecordError::Storage`] when a read or the
    /// fresh write fails.
    pub fn open(
        mut storage: S,
        copies: RecordCopies,
        layout: ImageLayout,
        handover: Handover,
    ) -> Result<(Self, Found), RecordError<S::Error>> {
        if !in_layout(&layout, handover.started) {
            return Err(RecordError::UnknownStartedSlot(SlotId(handover.started)));
        }
        let (a, b) = read_copies(&mut storage, copies)?;
        let one_valid = a.is_some() != b.is_some();
        let Some((newest_copy, record)) = newest(a, b) else {
            // No copy decodes. Passing copy B as the newest sends the first
            // write to copy A.
            let empty = Record {
                counter: 0,
                confirmed: handover.started,
                session: None,
            };
            return Self::rewrite(storage, copies, layout, handover, empty, CopyId::B)
                .map(|session| (session, Found::Rewritten(Rewrite::NoValidCopy)));
        };
        let rewrite = if !record.names_known_slots(|slot| in_layout(&layout, slot)) {
            Rewrite::UnknownSlot
        } else if !record.steers_to(handover.started) {
            Rewrite::Walked
        } else {
            // The image of the session sits in the trial slot of a pending
            // trial, and in the confirmed slot once the confirm wrote the
            // trial slot in as confirmed.
            let runs_session_image = match record.session.map(|session| session.phase) {
                Some(Phase::TrialPending { trial }) => handover.started == trial,
                Some(Phase::Committed { .. }) => handover.started == record.confirmed,
                _ => false,
            };
            let selected = if runs_session_image {
                BootSelection::Trial
            } else {
                BootSelection::Confirmed
            };
            let session = Self {
                storage,
                copies,
                layout,
                handover,
                selected,
                record,
                newest: newest_copy,
            };
            let found = if one_valid {
                Found::OneValidCopy
            } else {
                Found::Intact
            };
            return Ok((session, found));
        };
        Self::rewrite(storage, copies, layout, handover, record, newest_copy)
            .map(|session| (session, Found::Rewritten(rewrite)))
    }

    /// The slot the next [`prepare`](SelfUpdate::prepare) records as the
    /// trial slot, so board code writes the image there. `None` when every
    /// slot is the confirmed slot, the slot running now or the previous slot
    /// of a `Committed` session.
    #[must_use]
    pub fn next_trial_slot(&self) -> Option<SlotId> {
        pick_trial_slot(
            &self.record,
            self.handover.started,
            self.layout.slots().iter().map(|slot| slot.id().0),
        )
        .map(SlotId)
    }

    /// The trial slot of the session in flight. `None` with no session, and
    /// after the confirm, which made the trial slot the confirmed slot.
    #[must_use]
    pub fn trial_slot(&self) -> Option<SlotId> {
        self.record.session.and_then(Session::trial).map(SlotId)
    }

    /// Adopts the slot the boot stage started when no copy decodes, the
    /// record names a slot outside the layout, or the boot stage walked:
    /// writes the record after `base` with the started slot confirmed and no
    /// session.
    fn rewrite(
        storage: S,
        copies: RecordCopies,
        layout: ImageLayout,
        handover: Handover,
        base: Record,
        newest: CopyId,
    ) -> Result<Self, RecordError<S::Error>> {
        let mut session = Self {
            storage,
            copies,
            layout,
            handover,
            selected: BootSelection::Confirmed,
            record: base,
            newest,
        };
        session.write(handover.started, None)?;
        Ok(session)
    }

    /// Writes the record after the current one into the copy that does not
    /// hold it. Updates the record in RAM only once the write is durable.
    fn write(
        &mut self,
        confirmed: u8,
        session: Option<Session>,
    ) -> Result<(), RecordError<S::Error>> {
        let record = self.record.next(confirmed, session);
        let target = self.newest.other();
        self.storage.open()?;
        let written = write_and_wait(
            &mut self.storage,
            self.copies.offset(target),
            &record.encode(),
        );
        let closed = self.storage.close();
        written?;
        closed?;
        self.record = record;
        self.newest = target;
        Ok(())
    }

    /// Hands the storage back, for a test that simulates a reset.
    #[cfg(test)]
    pub(crate) fn into_storage(self) -> S {
        self.storage
    }

    #[cfg(test)]
    pub(crate) fn storage(&self) -> &S {
        &self.storage
    }

    #[cfg(test)]
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }
}

impl<S: Storage> SelfUpdate for RecordSession<S>
where
    S::Error: core::fmt::Debug,
{
    type Error = RecordError<S::Error>;

    fn state(&self) -> Result<SelfUpdateState, Self::Error> {
        Ok(state_of(&self.record))
    }

    fn running(&self) -> Result<RunningImage, Self::Error> {
        Ok(RunningImage {
            selected: self.selected,
            svn: Some(Svn(self.handover.svn)),
        })
    }

    fn prepare(&mut self, svn: Svn) -> Result<(), Self::Error> {
        let trial = self.next_trial_slot().ok_or(RecordError::NoFreeSlot)?;
        let session = Session {
            phase: Phase::Prepared { trial: trial.0 },
            svn: svn.0,
        };
        if self.record.session == Some(session) {
            return Ok(());
        }
        self.write(self.record.confirmed, Some(session))
    }

    fn set_trial_pending(&mut self) -> Result<(), Self::Error> {
        match self.record.session {
            Some(Session {
                phase: Phase::Prepared { trial },
                svn,
            }) => self.write(
                self.record.confirmed,
                Some(Session {
                    phase: Phase::TrialPending { trial },
                    svn,
                }),
            ),
            Some(Session {
                phase: Phase::TrialPending { .. },
                ..
            }) => Ok(()),
            _ => Err(RecordError::WrongState(state_of(&self.record))),
        }
    }

    fn confirm(&mut self) -> Result<(), Self::Error> {
        match self.record.session {
            // The trial slot becomes the confirmed slot, and the session
            // keeps the old confirmed slot so a revert can write it back.
            Some(Session {
                phase: Phase::TrialPending { trial },
                svn,
            }) => {
                if trial != self.handover.started {
                    return Err(RecordError::TrialNotRunning);
                }
                self.write(
                    trial,
                    Some(Session {
                        phase: Phase::Committed {
                            previous: self.record.confirmed,
                        },
                        svn,
                    }),
                )
            }
            Some(Session {
                phase: Phase::Committed { .. },
                ..
            }) => Ok(()),
            _ => Err(RecordError::WrongState(state_of(&self.record))),
        }
    }

    fn complete(&mut self) -> Result<(), Self::Error> {
        match self.record.session {
            Some(Session {
                phase: Phase::Committed { .. },
                ..
            }) => self.write(self.record.confirmed, None),
            None => Ok(()),
            Some(_) => Err(RecordError::WrongState(state_of(&self.record))),
        }
    }

    fn revert(&mut self) -> Result<(), Self::Error> {
        match self.record.session {
            Some(Session {
                phase: Phase::Committed { previous },
                ..
            }) => self.write(previous, None),
            Some(_) => self.write(self.record.confirmed, None),
            None => Ok(()),
        }
    }
}

/// The session state a record holds.
fn state_of(record: &Record) -> SelfUpdateState {
    match record.session {
        None => SelfUpdateState::Idle,
        Some(session) => {
            let svn = Svn(session.svn);
            match session.phase {
                Phase::Prepared { .. } => SelfUpdateState::Prepared { svn },
                Phase::TrialPending { .. } => SelfUpdateState::TrialPending { svn },
                Phase::Committed { .. } => SelfUpdateState::Committed { svn },
            }
        }
    }
}

fn in_layout(layout: &ImageLayout, slot: u8) -> bool {
    layout.slots().iter().any(|known| known.id().0 == slot)
}

/// Reads and decodes both copies in one storage session.
fn read_copies<S: Storage>(
    storage: &mut S,
    copies: RecordCopies,
) -> Result<(Option<Record>, Option<Record>), S::Error> {
    storage.open()?;
    let mut a = [0; RECORD_LEN];
    let mut b = [0; RECORD_LEN];
    let read = storage
        .read_at(copies.offset(CopyId::A), &mut a)
        .and_then(|()| storage.read_at(copies.offset(CopyId::B), &mut b));
    let closed = storage.close();
    read?;
    closed?;
    Ok((Record::decode(&a).ok(), Record::decode(&b).ok()))
}

/// Writes `bytes` at `offset` and polls until the write is durable.
fn write_and_wait<S: Storage>(
    storage: &mut S,
    offset: usize,
    bytes: &[u8],
) -> Result<(), S::Error> {
    storage.start_write(offset, bytes)?;
    while storage.is_busy() {
        core::hint::spin_loop();
    }
    storage.complete_write()
}
