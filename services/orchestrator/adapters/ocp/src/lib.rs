// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The orchestrator side of the OCP Recovery Agent.
//!
//! The Recovery Agent pushes images to a downstream device in a task
//! outside the orchestrator loop (`RecoveryAgent` in the agent crate). This
//! crate connects 1 managed device of the orchestrator with the Recovery
//! Agent task, through capabilities the orchestrator already has:
//!
//! - [`NotifyingBootControl`] wraps the `BootControl` of the reset line and
//!   posts a [`BootCyclePost`] on every hold and release.
//! - [`BootCycle::boot_status`] turns the report the task sent last into
//!   the `BootStatus` the board reads in its `EvidenceReader`, so the boot
//!   walk and its checkpoint timeout judge the push.
//!
//! The orchestrator loop calls nothing that waits for the task. A post is
//! one-way, and the loop takes a report when the task sends one. A lost
//! post or a task that stopped keeps the evidence at `Booting`, and the
//! checkpoint timeout of the 1 downstream device ends the wait.
//!
//! A board that uses this crate has these limits to meet:
//!
//! - The orchestrator releases a device only after the verifier of the
//!   board accepted the image of that device. So the eRoT has to store
//!   every image the Recovery Agent can push before the release, and the
//!   verifier of the board has to cover each of these images.
//! - The checkpoint window of the downstream device has to cover the push
//!   of every image, because each retry starts again from byte 0.
//! - A downstream device that reports no end state reads `Booting` until
//!   the checkpoint timeout. For such a device the board compares the count
//!   of activated images in `PushStatus::NoEndState`, from
//!   [`BootCycle::kept_status`], with the number of images the board
//!   expects, or reads the boot evidence from another signal of the device.
//! - OCP Recovery loads the first firmware stage. A downstream device that
//!   loads the rest over PLDM needs a second checkpoint after the push.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use core::cell::Cell;

use openprot_ocp_recovery_agent::{BootCyclePost, Outcome, PushReport, PushStatus};
use openprot_ocp_recovery_api::regs::{DeviceState, RecoveryState};
use orchestrator_capabilities::{BootControl, BootStatus};

/// The boot cycle counter of 1 downstream device, and the status the
/// Recovery Agent task reported for the current boot cycle.
///
/// The reset line, the report intake and the boot evidence of the device
/// share 1 `BootCycle` by reference, and all of them run in the
/// orchestrator loop, so the fields are `Cell`s. A `Cell` cannot sit in a
/// `static`, so the board creates the `BootCycle` in the function that
/// runs the orchestrator loop.
#[derive(Debug, Default)]
pub struct BootCycle {
    cycle: Cell<u32>,
    kept: Cell<Option<PushStatus>>,
}

impl BootCycle {
    /// A `BootCycle` before the first hold or release.
    pub const fn new() -> Self {
        Self {
            cycle: Cell::new(0),
            kept: Cell::new(None),
        }
    }

    /// Takes a report of the Recovery Agent task. A report of an earlier
    /// boot cycle describes a boot the reset line ended, so `accept` keeps
    /// only a report with the number of the current boot cycle.
    ///
    /// The orchestrator loop polls the boot walks right after `accept`,
    /// because a boot walk reads evidence only when polled and judges a
    /// lapsed window as a timeout before any read.
    pub fn accept(&self, report: PushReport) {
        if report.cycle == self.cycle.get() {
            self.kept.set(Some(report.status));
        }
    }

    /// The status of the report kept for the current boot cycle.
    pub fn kept_status(&self) -> Option<PushStatus> {
        self.kept.get()
    }

    /// The kept report as boot evidence, without a register access.
    /// `Booting` without a report.
    pub fn boot_status(&self) -> BootStatus {
        match self.kept.get() {
            None => BootStatus::Booting,
            Some(status) => boot_status(status),
        }
    }

    /// Counts a new boot cycle and drops the kept report, because the hold
    /// or the release ended the boot the kept report describes. Returns the
    /// new number.
    fn begin(&self) -> u32 {
        let cycle = self.cycle.get().wrapping_add(1);
        self.cycle.set(cycle);
        self.kept.set(None);
        cycle
    }
}

/// Where [`NotifyingBootControl`] posts to the Recovery Agent task.
///
/// `post` returns without a wait for the task, and a newer post replaces a
/// post the task has not taken yet. A post has no error: a post the task
/// never takes ends in the checkpoint timeout of the downstream device.
pub trait BootCyclePostSink {
    /// Posts `post` to the task.
    fn post(&mut self, post: BootCyclePost);
}

/// A `BootControl` for the reset line of a downstream device with a
/// Recovery Agent task.
///
/// Every hold and every release begins a new boot cycle in the
/// [`BootCycle`] and posts to the task, so the task starts a fresh
/// sequence for each release, and [`BootCycle::accept`] drops a report of
/// an earlier boot cycle.
pub struct NotifyingBootControl<'a, C, S> {
    inner: C,
    cycle: &'a BootCycle,
    sink: S,
}

impl<'a, C, S> NotifyingBootControl<'a, C, S> {
    /// Wraps `inner`, the `BootControl` that moves the reset line.
    pub fn new(inner: C, cycle: &'a BootCycle, sink: S) -> Self {
        Self { inner, cycle, sink }
    }
}

impl<C: BootControl, S: BootCyclePostSink> BootControl for NotifyingBootControl<'_, C, S> {
    type Error = C::Error;

    fn hold_in_reset(&mut self) -> Result<(), Self::Error> {
        let held = self.inner.hold_in_reset();
        self.cycle.begin();
        self.sink.post(BootCyclePost::Held);
        held
    }

    /// After a failed release the state of the reset line is unknown, so
    /// the post is `Held` and the task pushes nothing.
    fn release(&mut self) -> Result<(), Self::Error> {
        let released = self.inner.release();
        let cycle = self.cycle.begin();
        self.sink.post(match released {
            Ok(()) => BootCyclePost::Released { cycle },
            Err(_) => BootCyclePost::Held,
        });
        released
    }
}

/// The boot evidence for a status of the push.
///
/// A failure is fatal where another attempt with the same images cannot
/// change the result.
fn boot_status(status: PushStatus) -> BootStatus {
    match status {
        PushStatus::NoEndState { .. } => BootStatus::Booting,
        PushStatus::Ended(Outcome::Success) => BootStatus::Booted,
        PushStatus::Ended(Outcome::DeviceHealthy) => BootStatus::Booted,
        PushStatus::Ended(Outcome::Failed { state, .. }) => match state {
            RecoveryState::AuthenticationError => BootStatus::FailedFatal,
            RecoveryState::Failed => BootStatus::FailedRetriable,
            RecoveryState::EnterRecoveryError => BootStatus::FailedRetriable,
            RecoveryState::InvalidComponentAddressSpace => BootStatus::FailedRetriable,
            // `RecoveryState` is `#[non_exhaustive]`, so the match needs this
            // arm. The arm covers the states `FifoPush` reports as no
            // failure today, and every state a later version adds.
            _ => BootStatus::FailedRetriable,
        },
        PushStatus::Ended(Outcome::DeviceFailed { state }) => match state {
            DeviceState::FatalError => BootStatus::FailedFatal,
            DeviceState::BootFailure => BootStatus::FailedRetriable,
            // `DeviceState` is `#[non_exhaustive]`, so the match needs this
            // arm. The arm covers the states `FifoPush` reports as no
            // failure today, and every state a later version adds.
            _ => BootStatus::FailedRetriable,
        },
        PushStatus::ImageNotHeld { .. } => BootStatus::FailedFatal,
        PushStatus::ImageSize => BootStatus::FailedFatal,
        PushStatus::ImageLimit => BootStatus::FailedFatal,
        // The agent repeats the read, so the checkpoint window decides.
        PushStatus::ImageRead => BootStatus::Booting,
        PushStatus::RestartLimit => BootStatus::FailedRetriable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openprot_ocp_recovery_api::regs::ImageIndex;

    /// The reset line failed to move.
    #[derive(Debug, PartialEq, Eq)]
    struct LineFault;

    impl core::fmt::Display for LineFault {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("reset line fault")
        }
    }

    impl core::error::Error for LineFault {}

    /// What the inner `BootControl` did, in call order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LineCall {
        Hold,
        Release,
    }

    #[derive(Default)]
    struct MockLine {
        calls: Vec<LineCall>,
        fail: bool,
    }

    impl BootControl for &mut MockLine {
        type Error = LineFault;

        fn hold_in_reset(&mut self) -> Result<(), LineFault> {
            self.calls.push(LineCall::Hold);
            if self.fail {
                Err(LineFault)
            } else {
                Ok(())
            }
        }

        fn release(&mut self) -> Result<(), LineFault> {
            self.calls.push(LineCall::Release);
            if self.fail {
                Err(LineFault)
            } else {
                Ok(())
            }
        }
    }

    /// A sink that keeps every post. No task takes a post from it.
    #[derive(Default)]
    struct Posts(Vec<BootCyclePost>);

    impl BootCyclePostSink for &mut Posts {
        fn post(&mut self, post: BootCyclePost) {
            self.0.push(post);
        }
    }

    fn report(cycle: u32, status: PushStatus) -> PushReport {
        PushReport { cycle, status }
    }

    const SUCCESS: PushStatus = PushStatus::Ended(Outcome::Success);

    #[test]
    fn hold_and_release_move_the_inner_line_and_post_with_a_new_boot_cycle_number() {
        let cycle = BootCycle::new();
        let mut line = MockLine::default();
        let mut posts = Posts::default();
        let mut control = NotifyingBootControl::new(&mut line, &cycle, &mut posts);

        assert_eq!(control.hold_in_reset(), Ok(()));
        assert_eq!(control.release(), Ok(()));
        assert_eq!(control.release(), Ok(()));

        assert_eq!(
            line.calls,
            [LineCall::Hold, LineCall::Release, LineCall::Release]
        );
        // The hold counted boot cycle 1, so the releases carry 2 and 3.
        assert_eq!(
            posts.0,
            [
                BootCyclePost::Held,
                BootCyclePost::Released { cycle: 2 },
                BootCyclePost::Released { cycle: 3 },
            ]
        );
    }

    #[test]
    fn a_failed_hold_or_release_posts_held_and_returns_the_error_of_the_inner_line() {
        let cycle = BootCycle::new();
        let mut line = MockLine {
            fail: true,
            ..MockLine::default()
        };
        let mut posts = Posts::default();
        let mut control = NotifyingBootControl::new(&mut line, &cycle, &mut posts);

        assert_eq!(control.release(), Err(LineFault));
        assert_eq!(control.hold_in_reset(), Err(LineFault));

        assert_eq!(posts.0, [BootCyclePost::Held, BootCyclePost::Held]);
    }

    #[test]
    fn a_report_of_the_boot_cycle_before_reads_as_booting() {
        let cycle = BootCycle::new();
        let mut line = MockLine::default();
        let mut posts = Posts::default();
        let mut control = NotifyingBootControl::new(&mut line, &cycle, &mut posts);
        control.release().unwrap();
        let Some(&BootCyclePost::Released { cycle: first }) = posts.0.last() else {
            panic!("the release posts Released");
        };
        cycle.accept(report(first, SUCCESS));
        assert_eq!(cycle.boot_status(), BootStatus::Booted);

        // The next release drops the kept report, and `accept` ignores a
        // late report of the boot cycle before, because the report carries
        // another number than the current boot cycle.
        let mut control = NotifyingBootControl::new(&mut line, &cycle, &mut posts);
        control.release().unwrap();
        assert_eq!(cycle.boot_status(), BootStatus::Booting);
        cycle.accept(report(first, SUCCESS));
        assert_eq!(cycle.boot_status(), BootStatus::Booting);
        assert_eq!(cycle.kept_status(), None);
    }

    #[test]
    fn no_report_no_end_state_and_a_repeated_image_read_read_as_booting() {
        assert_eq!(BootCycle::new().boot_status(), BootStatus::Booting);
        for status in [
            PushStatus::NoEndState { activated: 2 },
            PushStatus::ImageRead,
        ] {
            assert_eq!(boot_status(status), BootStatus::Booting);
        }
    }

    #[test]
    fn success_and_a_healthy_device_read_as_booted() {
        assert_eq!(boot_status(SUCCESS), BootStatus::Booted);
        assert_eq!(
            boot_status(PushStatus::Ended(Outcome::DeviceHealthy)),
            BootStatus::Booted
        );
    }

    #[test]
    fn an_image_the_device_cannot_authenticate_reads_as_a_fatal_failure() {
        let failed = |state| {
            boot_status(PushStatus::Ended(Outcome::Failed {
                state,
                image: ImageIndex::FIRST,
            }))
        };
        assert_eq!(
            failed(RecoveryState::AuthenticationError),
            BootStatus::FailedFatal
        );
        for state in [
            RecoveryState::Failed,
            RecoveryState::EnterRecoveryError,
            RecoveryState::InvalidComponentAddressSpace,
        ] {
            assert_eq!(failed(state), BootStatus::FailedRetriable);
        }
    }

    #[test]
    fn a_fatal_error_of_the_device_reads_as_a_fatal_failure_and_a_boot_failure_as_retriable() {
        let failed = |state| boot_status(PushStatus::Ended(Outcome::DeviceFailed { state }));
        assert_eq!(failed(DeviceState::FatalError), BootStatus::FailedFatal);
        assert_eq!(
            failed(DeviceState::BootFailure),
            BootStatus::FailedRetriable
        );
    }

    #[test]
    fn an_image_the_agent_cannot_push_reads_as_a_fatal_failure() {
        for status in [
            PushStatus::ImageNotHeld { index: 1 },
            PushStatus::ImageSize,
            PushStatus::ImageLimit,
        ] {
            assert_eq!(boot_status(status), BootStatus::FailedFatal);
        }
    }

    #[test]
    fn the_restart_limit_reads_as_a_retriable_failure() {
        assert_eq!(
            boot_status(PushStatus::RestartLimit),
            BootStatus::FailedRetriable
        );
    }
}
