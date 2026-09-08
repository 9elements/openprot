// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The [`TrialBoot`] commit capability contract.
//!
//! How the verdict is reached differs by implementor.
//!
//! For a downstream device the eRoT watches from outside: [`BootWatch`]
//! judges the device's checkpoints and answers with a [`WalkVerdict`], read
//! through an [`EvidenceReader`] over the board's ready lines. `Complete`
//! means every checkpoint passed, so the caller confirms; `Failed`, window
//! expiry included, means the attempt died, so the caller reverts.
//!
//! For the eRoT's own image there is no outside observer. Success is the
//! trial image reaching the point where its own health checks pass and
//! calling `confirm`; if it can do that, it worked. Failure is never
//! getting there, and no code of ours is running to notice: whatever
//! resets the part next (a watchdog bounding the trial, a power cycle, a
//! panic) boots the confirmed slot, because the arming was consumed by the
//! trial boot. The fresh image then reads the verdict from durable state
//! alone: running the confirmed slot with a trial still pending means the
//! trial never confirmed. No reset-cause register is consulted. What the
//! eRoT's health check consists of is the caller's policy, not this
//! seam's.
//!
//! [`BootWatch`]: crate::BootWatch
//! [`WalkVerdict`]: crate::WalkVerdict
//! [`EvidenceReader`]: crate::EvidenceReader

/// Commit capability: resolve a tentatively activated image once its boot
/// has been judged.
///
/// [`Updatable::activate`](crate::Updatable::activate) proposes the staged
/// image as the boot candidate and never commits it. This trait is the
/// other half: promote that candidate after it proved itself, or drop it.
///
/// Implemented for every device whose slot selection the eRoT actuates.
/// That is downstream devices behind interposed flash and the eRoT's own
/// image alike; the two differ in who judges the boot, not in this
/// contract. A device that resolves its own slots (a PLDM firmware device
/// commits internally) has no `TrialBoot` on the eRoT side, the same split
/// as [`SvnFloor`](crate::SvnFloor). Slot identity stays behind the seam,
/// as it does in `Updatable`: the only resolvable trial is the one the last
/// activation armed, so there is nothing to name.
///
/// The arming applies to the next boot only; the trial record outlives it.
/// So an image that hangs, or an eRoT that loses power mid-window, runs the
/// confirmed slot again with no call from anyone; what enforces that (a
/// boot-select register cleared by the boot ROM, or equivalent) is the
/// implementor's mechanism. The record stays
/// [`is_pending`](Self::is_pending) until `confirm` or `revert`, so an
/// orchestrator that itself rebooted mid-update finds the trial and
/// resolves it instead of losing track of it.
///
/// When `confirm` returns `Ok` the promotion survives power loss. If power
/// is lost mid-call it never returns, so the next boot still finds the
/// trial pending and the caller confirms again; a torn write must never
/// leave the device booting an unconfirmed image.
///
/// Both calls are idempotent: with nothing pending they succeed as a no-op,
/// because a replayed resolution is harmless and the confirmed slot is
/// already the answer either call would have produced. A caller that needs
/// to tell a replay from a missing trial checks `is_pending` first, the same
/// shape as [`SvnFloor::advance`](crate::SvnFloor::advance). Neither call
/// boots anything: they move slot metadata only. Restarting a downstream
/// device is [`BootControl`](crate::BootControl); for the eRoT's own image
/// no call is needed, since the next reset, whatever its source, boots the
/// confirmed slot.
///
/// `is_pending` is a yes or no: it does not say whether the armed image has
/// booted yet. Nothing needs that distinction, because a half-done update
/// is restarted from scratch rather than resumed, so an unresolved trial is
/// reverted and rerun either way.
///
/// It follows that this seam cannot tell a `confirm` that followed an
/// observed boot from one that did not. Confirming only what was observed
/// is the caller's discipline, as holding a device in reset before
/// verifying it is in [`BootControl`](crate::BootControl).
/// Which slot the running image booted from is not available here either;
/// that answer comes from below the seam.
pub trait TrialBoot {
    /// The error type reported by this device's trial record.
    ///
    /// Bounded by [`core::error::Error`] so the caller gets `Display` and a
    /// `source()` cause chain, not just a `Debug` dump. Error categories
    /// are implementation-defined.
    type Error: core::error::Error;

    /// Whether an activated image is still awaiting its verdict.
    ///
    /// Answered from durable state: for the eRoT's own update the image
    /// that calls `confirm` is not the image that armed the trial, and an
    /// orchestrator that rebooted mid-update has no memory of a downstream
    /// device's pending trial either.
    fn is_pending(&self) -> Result<bool, Self::Error>;

    /// Promotes the activated image to confirmed and clears the trial.
    fn confirm(&mut self) -> Result<(), Self::Error>;

    /// Clears the trial without promoting; the confirmed slot stays
    /// confirmed.
    fn revert(&mut self) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct MockFault;

    impl core::fmt::Display for MockFault {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("mock trial record fault")
        }
    }

    impl core::error::Error for MockFault {}

    /// The verdict the caller reached for the boot it judged.
    #[derive(Copy, Clone)]
    enum Verdict {
        Healthy,
        Bad,
    }

    /// The one flow both implementors go through: apply the verdict to
    /// whatever the last activation armed, then check the record came out
    /// clear, or a later boot would still find a trial nobody owns.
    /// Generic over `TrialBoot`, so a passive downstream device and the
    /// eRoT itself are driven by the same code.
    fn apply_verdict<T: TrialBoot>(trial: &mut T, verdict: Verdict) -> Result<(), T::Error> {
        match verdict {
            Verdict::Healthy => trial.confirm()?,
            Verdict::Bad => trial.revert()?,
        }
        assert!(!trial.is_pending()?, "resolving must clear the record");
        Ok(())
    }

    /// A slot-selection record whose arming applies to the next boot only,
    /// as both a passive device's eRoT-held store and the eRoT's own
    /// durable record behave.
    struct SlotRecord {
        confirmed_slot: u8,
        trial_slot: Option<u8>,
        next_boot_armed: bool,
    }

    impl SlotRecord {
        fn new(confirmed_slot: u8) -> Self {
            Self {
                confirmed_slot,
                trial_slot: None,
                next_boot_armed: false,
            }
        }

        /// What `Updatable::activate` maps onto for this device.
        fn activate(&mut self, slot: u8) {
            self.trial_slot = Some(slot);
            self.next_boot_armed = true;
        }

        /// Boots the device and returns the slot it ran: the trial slot if
        /// the next boot is still armed, the confirmed slot otherwise.
        fn boot(&mut self) -> u8 {
            match self.trial_slot {
                Some(slot) if self.next_boot_armed => {
                    self.next_boot_armed = false;
                    slot
                }
                _ => self.confirmed_slot,
            }
        }
    }

    /// One `TrialBoot` over a record the caller owns. How long the instance
    /// lives is the only difference between the two cases: for a passive
    /// downstream device the eRoT holds it across the whole flow and judges
    /// the boot from outside, over the device's boot-complete line; for the
    /// eRoT's own update the image that confirms builds a fresh one over
    /// the durable record, because the instance that armed the trial went
    /// away with the previous boot. Implemented against no HAL at all: the
    /// contract must be satisfiable from any stack (mock, IPC proxy,
    /// simulator), and a HAL-bound `Error` type would stop this compiling.
    struct TrialRecord<'a> {
        record: &'a mut SlotRecord,
        fail: bool,
    }

    impl<'a> TrialRecord<'a> {
        fn over(record: &'a mut SlotRecord) -> Self {
            Self {
                record,
                fail: false,
            }
        }

        /// Every call faults, for the error test.
        fn faulty(record: &'a mut SlotRecord) -> Self {
            Self { record, fail: true }
        }
    }

    impl TrialBoot for TrialRecord<'_> {
        type Error = MockFault;

        fn is_pending(&self) -> Result<bool, MockFault> {
            if self.fail {
                return Err(MockFault);
            }
            Ok(self.record.trial_slot.is_some())
        }

        fn confirm(&mut self) -> Result<(), MockFault> {
            if self.fail {
                return Err(MockFault);
            }
            if let Some(slot) = self.record.trial_slot.take() {
                self.record.confirmed_slot = slot;
            }
            Ok(())
        }

        fn revert(&mut self) -> Result<(), MockFault> {
            if self.fail {
                return Err(MockFault);
            }
            self.record.trial_slot = None;
            Ok(())
        }
    }

    #[test]
    fn a_healthy_trial_becomes_the_confirmed_slot_of_a_passive_device() {
        let mut record = SlotRecord::new(0);
        let mut device = TrialRecord::over(&mut record);
        device.record.activate(1);

        assert_eq!(device.record.boot(), 1, "the trial slot runs first");
        apply_verdict(&mut device, Verdict::Healthy).unwrap();
        assert_eq!(device.record.boot(), 1, "and stays the default");
    }

    #[test]
    fn an_unresolved_passive_device_trial_falls_back_but_stays_pending() {
        let mut record = SlotRecord::new(0);
        let mut device = TrialRecord::over(&mut record);
        device.record.activate(1);

        assert_eq!(device.record.boot(), 1);
        assert_eq!(
            device.record.boot(),
            0,
            "the arming is one-shot: no confirm, no second trial boot"
        );
        assert_eq!(
            device.is_pending(),
            Ok(true),
            "the record outlives the fallback, so a rebooted caller finds it"
        );
        apply_verdict(&mut device, Verdict::Bad).unwrap();
        assert_eq!(device.record.boot(), 0);
    }

    #[test]
    fn the_erot_confirms_its_own_trial_from_the_boot_the_trial_started() {
        let mut record = SlotRecord::new(0);
        record.activate(1);

        // The boot that arming triggered. The armed image is now running,
        // and the instance that armed it is gone.
        assert_eq!(record.boot(), 1);

        let mut trial = TrialRecord::over(&mut record);
        assert_eq!(
            trial.is_pending(),
            Ok(true),
            "the fresh image learns it is on trial from durable state alone"
        );
        apply_verdict(&mut trial, Verdict::Healthy).unwrap();

        assert_eq!(record.confirmed_slot, 1);
        assert_eq!(record.boot(), 1);
    }

    #[test]
    fn an_erot_trial_that_never_confirms_falls_back_on_the_next_reset() {
        let mut record = SlotRecord::new(0);
        record.activate(1);

        assert_eq!(record.boot(), 1, "the trial image runs and hangs");
        assert_eq!(
            record.boot(),
            0,
            "the next reset runs the confirmed image, with no call from us"
        );

        let mut trial = TrialRecord::over(&mut record);
        assert_eq!(trial.is_pending(), Ok(true));
        apply_verdict(&mut trial, Verdict::Bad).unwrap();
    }

    #[test]
    fn a_replayed_resolution_is_a_noop() {
        let mut record = SlotRecord::new(0);
        record.activate(1);
        record.boot();

        let mut trial = TrialRecord::over(&mut record);
        apply_verdict(&mut trial, Verdict::Healthy).unwrap();
        apply_verdict(&mut trial, Verdict::Healthy).expect("confirm with nothing pending succeeds");
        apply_verdict(&mut trial, Verdict::Bad).expect("and so does revert");

        assert_eq!(
            record.confirmed_slot, 1,
            "neither replay moved the slot back"
        );
    }

    #[test]
    fn errors_surface_through_the_generic_seam() {
        let mut record = SlotRecord::new(0);
        let mut device = TrialRecord::faulty(&mut record);

        assert_eq!(apply_verdict(&mut device, Verdict::Healthy), Err(MockFault));
        assert_eq!(apply_verdict(&mut device, Verdict::Bad), Err(MockFault));
    }
}
