// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Which slot the boot stage starts, and which slot the next update takes.

use crate::mark::TriedMark;
use crate::record::{Phase, Record};

/// What the boot stage does at this boot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    /// Start `slot`.
    ///
    /// With a `mark`, `slot` is an untried trial: the boot stage writes the
    /// mark, reads it back and locks the mark region before the jump, then
    /// starts the trial watchdog. When the mark write or the read-back fails,
    /// the boot stage calls [`select`] again and passes the mark it failed to
    /// write. `select` then reads the trial as tried and starts the confirmed
    /// slot or walks.
    Slot {
        /// The slot to start.
        slot: u8,
        /// The mark to write first, for a trial.
        mark: Option<TriedMark>,
    },
    /// No slot passes its checks. The boot stage starts the golden image in
    /// recovery mode.
    Recovery,
}

/// Picks the slot to start.
///
/// `record` is the newest valid copy, `mark` the tried mark the boot stage
/// holds, `slots` the slot ids in layout order, and `passes` runs the
/// signature check and the SVN floor check on one slot. `select` returns
/// only a slot that passes `passes`, and calls `passes` at most once per
/// slot.
///
/// 1. For an untried `TrialPending` session, the boot stage starts the trial
///    slot, with a mark.
/// 2. Otherwise the boot stage starts the confirmed slot. After the confirm,
///    the confirmed slot holds the new image.
/// 3. When the slot from the steps above fails its checks, or no record
///    decodes, or the record names a slot outside `slots`, the boot stage
///    walks `slots` in order and starts the first slot that passes. The walk
///    skips the trial slot of a `TrialPending` session, which gets one try.
/// 4. When no slot passes, the boot stage starts the golden image in
///    recovery mode.
pub fn select<I>(
    record: Option<Record>,
    mark: Option<TriedMark>,
    slots: I,
    mut passes: impl FnMut(u8) -> bool,
) -> Start
where
    I: IntoIterator<Item = u8> + Clone,
{
    let record = record.filter(|record| {
        record.names_known_slots(|slot| slots.clone().into_iter().any(|known| known == slot))
    });
    // The slots the steps before the walk checked and saw fail, and the
    // trial slot that already had its try. The walk starts neither.
    let mut skip_trial = None;
    let mut skip_confirmed = None;
    if let Some(record) = record {
        if let Some(Phase::TrialPending { trial }) = record.session.map(|session| session.phase) {
            skip_trial = Some(trial);
            let tried = mark.is_some_and(|mark| mark.counter == record.counter);
            if !tried && passes(trial) {
                return Start::Slot {
                    slot: trial,
                    mark: Some(TriedMark {
                        counter: record.counter,
                    }),
                };
            }
        }
        if passes(record.confirmed) {
            return Start::Slot {
                slot: record.confirmed,
                mark: None,
            };
        }
        skip_confirmed = Some(record.confirmed);
    }
    for slot in slots {
        if Some(slot) != skip_trial && Some(slot) != skip_confirmed && passes(slot) {
            return Start::Slot { slot, mark: None };
        }
    }
    Start::Recovery
}

/// The slot a new update writes its image into: the first slot in layout
/// order that is not the confirmed slot of `record`, not the slot running
/// now, and not the previous slot of a `Committed` session.
///
/// The running slot differs from the confirmed slot while an unconfirmed
/// trial runs. Writing into the running slot would replace the image the
/// eRoT executes, and writing into the previous slot would replace the
/// old image, whose slot a revert from `Committed` writes back as
/// confirmed, so the pick skips both. `None` means no slot is left.
#[must_use]
pub fn pick_trial_slot(
    record: &Record,
    running: u8,
    slots: impl IntoIterator<Item = u8>,
) -> Option<u8> {
    let previous = match record.session.map(|session| session.phase) {
        Some(Phase::Committed { previous }) => Some(previous),
        _ => None,
    };
    slots
        .into_iter()
        .find(|&slot| slot != record.confirmed && slot != running && Some(slot) != previous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Session;

    const SLOTS: [u8; 2] = [0, 1];
    const THREE: [u8; 3] = [0, 1, 2];
    /// A slot id no layout in these tests holds.
    const UNKNOWN_SLOT: u8 = 5;
    /// The counter of the record under test.
    const COUNTER: u32 = 41;

    const fn record(confirmed: u8, phase: Option<Phase>) -> Record {
        Record {
            counter: COUNTER,
            confirmed,
            session: match phase {
                Some(phase) => Some(Session { phase, svn: 7 }),
                None => None,
            },
        }
    }

    const fn start(slot: u8) -> Start {
        Start::Slot { slot, mark: None }
    }

    const fn trial(slot: u8) -> Start {
        Start::Slot {
            slot,
            mark: Some(TriedMark { counter: COUNTER }),
        }
    }

    const TRIED: Option<TriedMark> = Some(TriedMark { counter: COUNTER });
    const STALE: Option<TriedMark> = Some(TriedMark {
        counter: COUNTER - 1,
    });

    struct Case {
        name: &'static str,
        record: Option<Record>,
        mark: Option<TriedMark>,
        slots: &'static [u8],
        failing: &'static [u8],
        expect: Start,
    }

    #[test]
    fn selects_the_decided_slot_in_every_case() {
        let prepared = Some(Phase::Prepared { trial: 1 });
        let pending = Some(Phase::TrialPending { trial: 1 });
        let committed = Some(Phase::Committed { previous: 0 });
        let cases = [
            Case {
                name: "no record: the first slot that passes",
                record: None,
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
            Case {
                name: "no record, first slot fails: the walk goes on",
                record: None,
                mark: None,
                slots: &SLOTS,
                failing: &[0],
                expect: start(1),
            },
            Case {
                name: "no record, every slot fails: recovery",
                record: None,
                mark: None,
                slots: &SLOTS,
                failing: &[0, 1],
                expect: Start::Recovery,
            },
            Case {
                name: "no session: the confirmed slot, not the first one",
                record: Some(record(1, None)),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(1),
            },
            Case {
                name: "prepared: the confirmed slot",
                record: Some(record(0, prepared)),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
            Case {
                name: "prepared, confirmed fails: the walk starts the trial slot",
                record: Some(record(0, prepared)),
                mark: None,
                slots: &SLOTS,
                failing: &[0],
                expect: start(1),
            },
            Case {
                name: "pending, no mark: the trial, with a mark",
                record: Some(record(0, pending)),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: trial(1),
            },
            Case {
                name: "pending, mark from an earlier record: the trial",
                record: Some(record(0, pending)),
                mark: STALE,
                slots: &SLOTS,
                failing: &[],
                expect: trial(1),
            },
            Case {
                name: "pending, already tried: the confirmed slot",
                record: Some(record(0, pending)),
                mark: TRIED,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
            Case {
                name: "pending, trial fails its checks: the confirmed slot",
                record: Some(record(0, pending)),
                mark: None,
                slots: &SLOTS,
                failing: &[1],
                expect: start(0),
            },
            Case {
                name: "pending and tried, confirmed fails: the walk skips the trial",
                record: Some(record(0, pending)),
                mark: TRIED,
                slots: &SLOTS,
                failing: &[0],
                expect: Start::Recovery,
            },
            Case {
                name: "pending and tried, confirmed fails: the walk finds a third slot",
                record: Some(record(0, pending)),
                mark: TRIED,
                slots: &THREE,
                failing: &[0],
                expect: start(2),
            },
            Case {
                name: "committed: the confirmed slot, which holds the new image",
                record: Some(record(1, committed)),
                mark: TRIED,
                slots: &SLOTS,
                failing: &[],
                expect: start(1),
            },
            Case {
                name: "committed, confirmed fails: the walk starts the previous slot",
                record: Some(record(1, committed)),
                mark: None,
                slots: &SLOTS,
                failing: &[1],
                expect: start(0),
            },
            Case {
                name: "committed, confirmed fails: the walk starts a slot before the previous slot",
                record: Some(record(2, Some(Phase::Committed { previous: 1 }))),
                mark: None,
                slots: &THREE,
                failing: &[2],
                expect: start(0),
            },
            Case {
                name: "committed, both fail: recovery",
                record: Some(record(1, committed)),
                mark: None,
                slots: &SLOTS,
                failing: &[0, 1],
                expect: Start::Recovery,
            },
            Case {
                name: "confirmed slot fails: the walk",
                record: Some(record(1, None)),
                mark: None,
                slots: &SLOTS,
                failing: &[1],
                expect: start(0),
            },
            Case {
                name: "record names an unknown confirmed slot: the walk",
                record: Some(record(UNKNOWN_SLOT, None)),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
            Case {
                name: "record names an unknown trial slot: the walk",
                record: Some(record(
                    1,
                    Some(Phase::TrialPending {
                        trial: UNKNOWN_SLOT,
                    }),
                )),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
            Case {
                name: "record names an unknown previous slot: the walk",
                record: Some(record(
                    1,
                    Some(Phase::Committed {
                        previous: UNKNOWN_SLOT,
                    }),
                )),
                mark: None,
                slots: &SLOTS,
                failing: &[],
                expect: start(0),
            },
        ];
        for case in cases {
            let mut checked = [0; THREE.len()];
            let got = select(case.record, case.mark, case.slots.iter().copied(), |slot| {
                checked[usize::from(slot)] += 1;
                !case.failing.contains(&slot)
            });
            assert_eq!(got, case.expect, "{}", case.name);
            assert!(
                checked.iter().all(|&count| count <= 1),
                "{}: a slot was checked twice",
                case.name
            );
        }
    }

    #[test]
    fn the_trial_slot_is_the_first_free_one() {
        assert_eq!(pick_trial_slot(&record(0, None), 0, SLOTS), Some(1));
        assert_eq!(pick_trial_slot(&record(1, None), 1, SLOTS), Some(0));
        assert_eq!(pick_trial_slot(&record(0, None), 0, THREE), Some(1));
    }

    // An unconfirmed trial runs from slot 1 while slot 0 stays confirmed.
    // The next update must not overwrite either.
    #[test]
    fn the_trial_slot_skips_the_running_slot() {
        assert_eq!(pick_trial_slot(&record(0, None), 1, SLOTS), None);
        assert_eq!(pick_trial_slot(&record(0, None), 1, THREE), Some(2));
    }

    // After the confirm, slot 1 is confirmed and runs, and slot 0 holds the
    // image a revert writes back. The next update must not overwrite slot 0.
    #[test]
    fn the_trial_slot_skips_the_previous_slot() {
        let committed = record(1, Some(Phase::Committed { previous: 0 }));
        assert_eq!(pick_trial_slot(&committed, 1, SLOTS), None);
        assert_eq!(pick_trial_slot(&committed, 1, THREE), Some(2));
    }
}
