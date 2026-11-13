// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

use orchestrator_capabilities::{
    trial_outcome, BootSelection, RunningImage, SelfUpdate, SelfUpdateState, Svn, TrialOutcome,
};
use orchestrator_config::{ImageLayout, Region, Slot, SlotId};
use orchestrator_self_update_format::{
    newest, select, Handover, Record, Start, TriedMark, RECORD_LEN,
};
use util_io::Storage;

use crate::{Found, RecordCopies, RecordError, RecordSession, Rewrite};

/// The erase block of the RAM flash.
const BLOCK: usize = 64;
/// An erase block per record copy.
const FLASH_LEN: usize = 2 * BLOCK;
const COPY_A: usize = 0;
const COPY_B: usize = BLOCK;
const COPIES: RecordCopies = RecordCopies::new(COPY_A, COPY_B, BLOCK);
/// How many polls of `is_busy` answer `true` before a write finishes.
const POLLS_PER_WRITE: usize = 2;
/// A slot id neither layout in these tests holds.
const UNKNOWN_SLOT: u8 = 5;

/// The image slots, each in a separate region.
const SLOT_LEN: u32 = 0x1_0000;
static TWO: [Slot; 2] = [
    Slot::new(SlotId(0), Region::new(0, SLOT_LEN)),
    Slot::new(SlotId(1), Region::new(SLOT_LEN, SLOT_LEN)),
];
static THREE: [Slot; 3] = [
    Slot::new(SlotId(0), Region::new(0, SLOT_LEN)),
    Slot::new(SlotId(1), Region::new(SLOT_LEN, SLOT_LEN)),
    Slot::new(SlotId(2), Region::new(2 * SLOT_LEN, SLOT_LEN)),
];
const LAYOUT: ImageLayout = ImageLayout::new(&TWO, None);
const LAYOUT_THREE: ImageLayout = ImageLayout::new(&THREE, None);

/// The SVN of the image on the part before the update.
const OLD: Svn = Svn(3);
/// The SVN of the update.
const NEW: Svn = Svn(7);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FlashError {
    NoSession,
    AlreadyOpen,
    Busy,
    NotBusy,
    OutOfBounds,
    WrittenTwice,
    /// The write failed before it touched the flash.
    Refused,
    /// Power went away during the write.
    PowerLost,
}

/// RAM that behaves like NOR flash behind [`Storage`]: the first write into
/// an erase block in a session erases the whole block, and no byte takes a
/// second write in one session.
struct RamFlash {
    bytes: [u8; FLASH_LEN],
    open: bool,
    busy_polls: Option<usize>,
    erased: [bool; FLASH_LEN / BLOCK],
    written: [bool; FLASH_LEN],
    /// Counts the `start_write` calls the flash accepted.
    writes: usize,
    /// Refuses the next `start_write`.
    refuse_next: bool,
    /// Cuts the next write after this many bytes and fails it.
    cut_next: Option<usize>,
    cut: bool,
}

impl RamFlash {
    fn erased() -> Self {
        Self {
            bytes: [0xFF; FLASH_LEN],
            open: false,
            busy_polls: None,
            erased: [false; FLASH_LEN / BLOCK],
            written: [false; FLASH_LEN],
            writes: 0,
            refuse_next: false,
            cut_next: None,
            cut: false,
        }
    }

    /// Power goes away and comes back: the flash keeps its bytes and loses
    /// the session.
    fn power_cycle(mut self) -> Self {
        self.open = false;
        self.busy_polls = None;
        self.cut = false;
        self
    }
}

impl Storage for RamFlash {
    type Error = FlashError;

    fn open(&mut self) -> Result<(), FlashError> {
        if self.open {
            return Err(FlashError::AlreadyOpen);
        }
        self.open = true;
        self.erased = [false; FLASH_LEN / BLOCK];
        self.written = [false; FLASH_LEN];
        Ok(())
    }

    fn read_at(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), FlashError> {
        if !self.open {
            return Err(FlashError::NoSession);
        }
        if self.busy_polls.is_some() {
            return Err(FlashError::Busy);
        }
        let end = offset + buf.len();
        buf.copy_from_slice(self.bytes.get(offset..end).ok_or(FlashError::OutOfBounds)?);
        Ok(())
    }

    fn start_write(&mut self, offset: usize, data: &[u8]) -> Result<(), FlashError> {
        if !self.open {
            return Err(FlashError::NoSession);
        }
        if self.busy_polls.is_some() {
            return Err(FlashError::Busy);
        }
        if core::mem::take(&mut self.refuse_next) {
            return Err(FlashError::Refused);
        }
        let end = offset + data.len();
        if end > FLASH_LEN {
            return Err(FlashError::OutOfBounds);
        }
        if self.written[offset..end].iter().any(|&w| w) {
            return Err(FlashError::WrittenTwice);
        }
        for block in offset / BLOCK..=(end - 1) / BLOCK {
            if !self.erased[block] {
                self.bytes[block * BLOCK..(block + 1) * BLOCK].fill(0xFF);
                self.erased[block] = true;
            }
        }
        let kept = self.cut_next.take().map_or(data.len(), |cut| {
            self.cut = true;
            cut
        });
        self.bytes[offset..offset + kept].copy_from_slice(&data[..kept]);
        self.written[offset..end].fill(true);
        self.writes += 1;
        self.busy_polls = Some(POLLS_PER_WRITE);
        Ok(())
    }

    fn is_busy(&mut self) -> bool {
        match self.busy_polls {
            Some(0) | None => false,
            Some(left) => {
                self.busy_polls = Some(left - 1);
                true
            }
        }
    }

    fn complete_write(&mut self) -> Result<(), FlashError> {
        match self.busy_polls {
            Some(0) => {
                self.busy_polls = None;
                if self.cut {
                    Err(FlashError::PowerLost)
                } else {
                    Ok(())
                }
            }
            Some(_) => Err(FlashError::Busy),
            None => Err(FlashError::NotBusy),
        }
    }

    fn close(&mut self) -> Result<(), FlashError> {
        if !self.open {
            return Err(FlashError::NoSession);
        }
        if self.busy_polls.is_some() {
            return Err(FlashError::Busy);
        }
        self.open = false;
        Ok(())
    }
}

/// The boot stage started `slot` at `svn`.
const fn started(slot: u8, svn: Svn) -> Handover {
    Handover {
        started: slot,
        svn: svn.0,
    }
}

/// The session after a boot of `handover` on `flash`.
fn boot(flash: RamFlash, handover: Handover) -> (RecordSession<RamFlash>, Found) {
    RecordSession::open(flash.power_cycle(), COPIES, LAYOUT, handover).expect("the record opens")
}

/// Resets the part: the storage keeps its bytes and the boot stage starts
/// `handover`.
fn reset(session: RecordSession<RamFlash>, handover: Handover) -> RecordSession<RamFlash> {
    boot(session.into_storage(), handover).0
}

/// The boot stage on `flash`: decodes both copies, picks the slot with
/// `select`, keeps the tried mark of a trial in `mark`, and hands over the
/// started slot with the SVN of the image in that slot. Every slot passes its
/// checks.
fn boot_stage(flash: &RamFlash, mark: &mut Option<TriedMark>, images: &[Svn]) -> Handover {
    let copy = |at: usize| {
        let mut bytes = [0; RECORD_LEN];
        bytes.copy_from_slice(&flash.bytes[at..at + RECORD_LEN]);
        Record::decode(&bytes).ok()
    };
    let record = newest(copy(COPY_A), copy(COPY_B)).map(|(_, record)| record);
    let slots = LAYOUT.slots().iter().map(|slot| slot.id().0);
    match select(record, *mark, slots, |_| true) {
        Start::Slot { slot, mark: tried } => {
            if tried.is_some() {
                *mark = tried;
            }
            started(slot, images[usize::from(slot)])
        }
        Start::Recovery => panic!("every slot passes, so the boot stage starts one"),
    }
}

/// Resets the part and lets the boot stage pick the slot.
fn reboot(
    session: RecordSession<RamFlash>,
    mark: &mut Option<TriedMark>,
    images: &[Svn],
) -> RecordSession<RamFlash> {
    let flash = session.into_storage();
    let handover = boot_stage(&flash, mark, images);
    boot(flash, handover).0
}

/// A part that has run slot 0 at the old SVN and holds an intact record.
fn settled_part() -> RecordSession<RamFlash> {
    let (mut session, _) = boot(RamFlash::erased(), started(0, OLD));
    // The fresh record filled copy A. The writes below fill copy B, then
    // copy A again, so both copies decode.
    session.prepare(NEW).expect("prepare");
    session.revert().expect("revert");
    session
}

#[test]
fn a_new_part_writes_a_fresh_record_for_the_started_slot() {
    let (session, found) = boot(RamFlash::erased(), started(1, OLD));

    assert_eq!(found, Found::Rewritten(Rewrite::NoValidCopy));
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(0)));

    let (_, found) = boot(session.into_storage(), started(1, OLD));
    assert_eq!(found, Found::OneValidCopy, "one write fills one copy");
}

#[test]
fn an_update_runs_its_trial_and_completes_across_resets() {
    let mut images = [OLD, OLD];
    let mut mark = None;
    let mut session = settled_part();
    assert_eq!(session.next_trial_slot(), Some(SlotId(1)));
    session.prepare(NEW).expect("prepare");
    images[1] = NEW;
    session.set_trial_pending().expect("mark pending");
    assert_eq!(session.trial_slot(), Some(SlotId(1)));

    let mut session = reboot(session, &mut mark, &images);
    let state = session.state().expect("state");
    assert_eq!(state, SelfUpdateState::TrialPending { svn: NEW });
    assert_eq!(session.running(), Ok(RunningImage::trial(NEW)));
    assert_eq!(
        trial_outcome(state, session.running().expect("running")),
        TrialOutcome::InProgress
    );
    session.confirm().expect("confirm");

    assert_eq!(
        session.running(),
        Ok(RunningImage::trial(NEW)),
        "the answer holds for the whole boot"
    );
    assert_eq!(
        session.trial_slot(),
        None,
        "the confirm moved the trial slot"
    );

    // A reset before the SVN floor advance. The confirm wrote slot 1 in as
    // the confirmed slot, so the boot stage starts slot 1 with no mark.
    // Slot 1 holds the image of the session until the complete, so
    // `running` reports the trial.
    let mut session = reboot(session, &mut mark, &images);
    let state = session.state().expect("state");
    assert_eq!(state, SelfUpdateState::Committed { svn: NEW });
    assert_eq!(session.running(), Ok(RunningImage::trial(NEW)));
    assert_eq!(
        trial_outcome(state, session.running().expect("running")),
        TrialOutcome::ConfirmedUncommitted { svn: NEW }
    );
    session.complete().expect("complete");

    let session = reboot(session, &mut mark, &images);
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(NEW)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(0)));
}

// The trial boot ends in a reset before the confirm. The tried mark makes
// the boot stage start the confirmed slot, and settlement reverts.
#[test]
fn a_trial_that_fell_back_reads_unconfirmed_and_reverts() {
    let mut images = [OLD, OLD];
    let mut mark = None;
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    images[1] = NEW;
    session.set_trial_pending().expect("mark pending");
    let session = reboot(session, &mut mark, &images);
    assert_eq!(session.running(), Ok(RunningImage::trial(NEW)));

    let mut session = reboot(session, &mut mark, &images);
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
    let state = session.state().expect("state");
    assert_eq!(
        trial_outcome(state, session.running().expect("running")),
        TrialOutcome::Unconfirmed
    );
    session.revert().expect("revert");
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.next_trial_slot(), Some(SlotId(1)));
}

// The revert writes the previous slot from the Committed session back as
// the confirmed slot, so at the next boot the boot stage starts the old
// image.
#[test]
fn a_revert_from_committed_restores_the_previous_slot() {
    let mut images = [OLD, OLD];
    let mut mark = None;
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    images[1] = NEW;
    session.set_trial_pending().expect("mark pending");
    let mut session = reboot(session, &mut mark, &images);
    session.confirm().expect("confirm");
    session.revert().expect("revert");

    let session = reboot(session, &mut mark, &images);
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(1)));
}

// After the confirm, slot 1 runs at an SVN other than the session SVN, as
// after a writer with flash access replaced the record. `running` still
// reports the trial, so settlement reverts and reports
// `UnclaimedImageRunning`. At the next reset the boot stage starts slot 0.
#[test]
fn a_committed_session_at_another_svn_reports_the_trial_and_reverts() {
    let images = [OLD, NEW];
    let mut mark = None;
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    session.confirm().expect("confirm");

    let mut session = reset(session, started(1, OLD));
    let running = session.running().expect("running");
    assert_eq!(running, RunningImage::trial(OLD));
    assert_eq!(
        trial_outcome(session.state().expect("state"), running),
        TrialOutcome::Unconfirmed
    );
    session.revert().expect("revert");

    let session = reboot(session, &mut mark, &images);
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
}

// After the confirm, slot 0 holds the image a revert writes back, and slot
// 1 is confirmed and runs. On `LAYOUT` no slot is left for a new update.
#[test]
fn prepare_after_the_confirm_keeps_the_previous_slot() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    session.confirm().expect("confirm");

    assert_eq!(session.next_trial_slot(), None);
    assert_eq!(session.prepare(NEW), Err(RecordError::NoFreeSlot));
}

#[test]
fn a_repeated_confirm_writes_nothing() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    session.confirm().expect("confirm");
    session.storage_mut().writes = 0;

    session.confirm().expect("repeat confirm");
    assert_eq!(session.into_storage().writes, 0);
}

// The trial had its try and the boot stage started the confirmed slot. A
// confirm in that boot would write slot 1 in as confirmed, and the image in
// slot 1 did not run.
#[test]
fn confirm_is_refused_when_the_trial_slot_does_not_run() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(0, OLD));

    assert_eq!(session.confirm(), Err(RecordError::TrialNotRunning));
    assert_eq!(
        session.state(),
        Ok(SelfUpdateState::TrialPending { svn: NEW })
    );
}

#[test]
fn a_repeated_transition_writes_nothing() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut flash = session.into_storage();
    flash.writes = 0;
    let (mut session, _) = boot(flash, started(0, OLD));

    session.set_trial_pending().expect("repeat");
    session.revert().expect("revert");
    session.revert().expect("repeat revert");
    session.complete().expect("complete from Idle");

    assert_eq!(
        session.into_storage().writes,
        1,
        "only the first revert writes"
    );
}

#[test]
fn a_transition_from_the_wrong_state_is_refused() {
    let mut session = settled_part();
    assert_eq!(
        session.set_trial_pending(),
        Err(RecordError::WrongState(SelfUpdateState::Idle))
    );
    assert_eq!(
        session.confirm(),
        Err(RecordError::WrongState(SelfUpdateState::Idle))
    );
    session.prepare(NEW).expect("prepare");
    assert_eq!(
        session.complete(),
        Err(RecordError::WrongState(SelfUpdateState::Prepared {
            svn: NEW
        }))
    );
}

// An unclaimed trial runs from slot 1 while slot 0 stays confirmed. In
// `LAYOUT`, slot 0 is confirmed and slot 1 runs, so `prepare` has no slot to
// write the image into.
#[test]
fn prepare_refuses_to_overwrite_the_running_slot() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    session.revert().expect("revert");
    assert_eq!(session.running(), Ok(RunningImage::trial(NEW)));

    assert_eq!(session.next_trial_slot(), None);
    assert_eq!(session.prepare(NEW), Err(RecordError::NoFreeSlot));
}

// The confirmed slot failed its checks while the session read Prepared, and
// the walk started the trial slot. The eRoT writes the trial slot in as
// confirmed, so the next update finds a free slot without another reset.
#[test]
fn a_walk_to_a_prepared_trial_slot_rewrites_the_record() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    let flash = session.into_storage().power_cycle();

    let (session, found) = boot(flash, started(1, NEW));
    assert_eq!(found, Found::Rewritten(Rewrite::Walked));
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(NEW)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(0)));
}

// The slots the record names failed their checks and the boot stage walked
// to slot 2. The eRoT writes slot 2 in as confirmed and drops the session.
#[test]
fn a_walk_to_another_slot_rewrites_the_record() {
    let (mut session, _) =
        RecordSession::open(RamFlash::erased(), COPIES, LAYOUT_THREE, started(0, OLD))
            .expect("the record opens");
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let flash = session.into_storage().power_cycle();

    let (session, found) = RecordSession::open(flash, COPIES, LAYOUT_THREE, started(2, OLD))
        .expect("the record opens");
    assert_eq!(found, Found::Rewritten(Rewrite::Walked));
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(0)));
}

// A record written for `LAYOUT_THREE` names slot 2, which `LAYOUT` does not
// have.
#[test]
fn a_record_naming_an_unknown_slot_is_rewritten() {
    let (session, _) =
        RecordSession::open(RamFlash::erased(), COPIES, LAYOUT_THREE, started(2, OLD))
            .expect("the record opens");
    let (session, found) = boot(session.into_storage(), started(0, OLD));

    assert_eq!(found, Found::Rewritten(Rewrite::UnknownSlot));
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
}

#[test]
fn a_handover_naming_an_unknown_slot_is_refused() {
    let opened = RecordSession::open(
        RamFlash::erased(),
        COPIES,
        LAYOUT,
        started(UNKNOWN_SLOT, OLD),
    );
    assert!(matches!(
        opened,
        Err(RecordError::UnknownStartedSlot(SlotId(UNKNOWN_SLOT)))
    ));
}

// A refused write changes neither the flash nor the record in RAM, so the
// retry writes the same copy.
#[test]
fn a_failed_write_keeps_the_record_and_the_retry_succeeds() {
    let mut session = settled_part();
    session.storage_mut().refuse_next = true;

    assert_eq!(
        session.prepare(NEW),
        Err(RecordError::Storage(FlashError::Refused))
    );
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));

    session.prepare(NEW).expect("retry");
    let (session, found) = boot(session.into_storage(), started(0, OLD));
    assert_eq!(found, Found::Intact);
    assert_eq!(session.state(), Ok(SelfUpdateState::Prepared { svn: NEW }));
}

/// One transition of a full update, from the state before it.
type Step = fn(&mut RecordSession<RamFlash>) -> Result<(), RecordError<FlashError>>;

// Power goes away during each transition, at every byte of the write. After
// the reset the state is the one before the transition, or the one after it
// when the flash holds the whole copy.
#[test]
fn a_torn_write_leaves_the_state_before_or_after() {
    // The update path, and the boot each step runs on.
    let path: [(Step, Handover); 4] = [
        (|s| s.prepare(NEW), started(0, OLD)),
        (|s| s.set_trial_pending(), started(0, OLD)),
        (|s| s.confirm(), started(1, NEW)),
        (|s| s.complete(), started(1, NEW)),
    ];
    // Each step under test, with how many steps of the path run before it.
    // The revert runs from Committed, the last state that holds a session.
    let revert: (Step, Handover) = (|s| s.revert(), started(1, NEW));
    // The third value is the boot the boot stage starts after the step
    // succeeds: slot 1 from the pending trial on, first as the trial slot and
    // after the confirm as the confirmed slot, and slot 0 after the revert,
    // which writes slot 0 back as the confirmed slot.
    let cases = [
        (path[0], 0, started(0, OLD)),
        (path[1], 1, started(1, NEW)),
        (path[2], 2, started(1, NEW)),
        (path[3], 3, started(1, NEW)),
        (revert, 3, started(0, OLD)),
    ];
    for (index, &((step, handover), earlier_steps, next_boot)) in cases.iter().enumerate() {
        for cut in 0..=RECORD_LEN {
            let mut session = settled_part();
            for &(earlier, earlier_boot) in &path[..earlier_steps] {
                session = reset(session, earlier_boot);
                earlier(&mut session).expect("an earlier step");
            }
            let mut session = reset(session, handover);
            let before = session.state().expect("state");
            let mut after = reset_copy(&session, handover);
            step(&mut after).expect("the step without a cut");
            let after = after.state().expect("state");

            session.storage_mut().cut_next = Some(cut);
            assert!(step(&mut session).is_err(), "step {index}, cut {cut}");
            let (mut session, _) = boot(session.into_storage(), handover);
            let state = session.state().expect("state");
            if cut < RECORD_LEN {
                assert_eq!(state, before, "step {index}, cut {cut}");
                // The retry lands on the torn copy and repairs it.
                step(&mut session).expect("the retry");
                let (session, found) = boot(session.into_storage(), next_boot);
                assert_eq!(found, Found::Intact, "step {index}, cut {cut}");
                assert_eq!(session.state(), Ok(after), "step {index}, cut {cut}");
            } else {
                assert_eq!(state, after, "step {index}, whole copy");
            }
        }
    }
}

/// A second session on a copy of the flash under `session`, to see what a
/// step does without the cut.
fn reset_copy(session: &RecordSession<RamFlash>, handover: Handover) -> RecordSession<RamFlash> {
    let mut flash = RamFlash::erased();
    flash.bytes = session.storage().bytes;
    boot(flash, handover).0
}

// After a revert in the trial boot, the record holds no session and the
// trial slot still runs. `running` keeps reporting the trial, so
// `trial_outcome` reads the `Idle` session with a running trial as
// unconfirmed.
#[test]
fn running_reports_the_trial_for_the_whole_trial_boot() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    assert_eq!(session.running(), Ok(RunningImage::trial(NEW)));

    session.revert().expect("revert");
    let running = session.running().expect("running");
    assert_eq!(running.selected, BootSelection::Trial);
    assert_eq!(
        trial_outcome(session.state().expect("state"), running),
        TrialOutcome::Unconfirmed
    );
}

// After the confirm, the signature check or the SVN floor check rejected
// slot 1, and the walk started the previous slot. The eRoT writes slot 0 in as confirmed and drops the
// session, so settlement sees no session and the SVN floor keeps the SVN it
// has.
#[test]
fn a_walk_to_the_previous_slot_rewrites_the_record() {
    let mut session = settled_part();
    session.prepare(NEW).expect("prepare");
    session.set_trial_pending().expect("mark pending");
    let mut session = reset(session, started(1, NEW));
    session.confirm().expect("confirm");

    let (session, found) = boot(session.into_storage(), started(0, OLD));
    assert_eq!(found, Found::Rewritten(Rewrite::Walked));
    assert_eq!(session.state(), Ok(SelfUpdateState::Idle));
    assert_eq!(session.running(), Ok(RunningImage::confirmed(OLD)));
    assert_eq!(session.next_trial_slot(), Some(SlotId(1)));
}
