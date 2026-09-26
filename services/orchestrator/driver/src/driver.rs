// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The [`PlatformDriver`]: one executor method per [`Effect`] variant, routed from
//! the SM through the [`Platform`] impl.

use openprot_orchestrator_sm::{
    BootFailureKind, ComponentId, ComponentKind, Effect, EffectError, Event, Orchestrator, Platform,
};

use crate::board::{
    Board, BoardCapabilities, ImageSource, Report, ReportSink, SvnFloorBinding, Verdict, Verifier,
};
use orchestrator_capabilities::{
    BootControl, BootWatch, FailureCause, IncrementalVerifier, PayloadSource, PayloadWindow,
    PollOutcome, Progress, Recovery, RestoreOutcome, StageProgress, Svn, SvnFloor, Updatable,
    VerifySession, WalkVerdict,
};

/// Why the driver could not carry out an effect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverError {
    /// The effect names a component the driver has no device for.
    UnknownComponent,
    /// The component's image source could not be opened.
    ImageUnavailable,
    /// Verify was asked for a component whose image was never staged.
    NotStaged,
    /// The verifier could not perform the check (a failed image is a
    /// [`Verdict`], not an error).
    VerifierFault,
    /// The component's boot control could not actuate the reset line.
    BootControlFault,
    /// A floor commit was asked for a component with no verified image —
    /// the SVN to advance to is unknown; fail closed.
    NoVerifiedImage,
    /// The component's SVN floor could not be advanced.
    SvnFloorFault,
    /// An update is already in flight; the frontend answers the requester
    /// over its own protocol, the SM never sees the refused request.
    UpdateBusy,
    /// The device refused to activate what it staged.
    UpdateFault,
    /// The recovery mechanism faulted (bus error, unreachable source).
    /// Distinct from source exhaustion, which is a verdict, not a fault.
    RecoveryFault,
    /// An update effect ran with no job recorded, or with the job in the
    /// wrong phase. The frontend records the job before the SM sees
    /// `UpdateRequest`, so this means the two have drifted apart.
    NoUpdateJob,
    /// The candidate does not fit the staging region the board wired, so
    /// there is nothing well-formed to authenticate.
    CandidateOutOfRange,
}

impl core::fmt::Display for DriverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DriverError::UnknownComponent => "no device for this component id",
            DriverError::ImageUnavailable => "image source could not be opened",
            DriverError::NotStaged => "no image staged for this component",
            DriverError::VerifierFault => "verifier could not perform the check",
            DriverError::BootControlFault => "boot control could not actuate the reset",
            DriverError::NoVerifiedImage => "no verified image to commit the floor to",
            DriverError::SvnFloorFault => "svn floor could not be advanced",
            DriverError::UpdateBusy => "an update is already in flight",
            DriverError::UpdateFault => "device refused to activate the staged image",
            DriverError::RecoveryFault => "recovery mechanism faulted",
            DriverError::NoUpdateJob => "no update job for this effect",
            DriverError::CandidateOutOfRange => "candidate does not fit the staging region",
        })
    }
}

impl core::error::Error for DriverError {}

/// The effect executors. Everything device-specific lives in the [`Board`];
/// the driver's own fields are bookkeeping.
pub struct PlatformDriver<B: BoardCapabilities, const N: usize> {
    board: Board<B, N>,
    /// Component whose image is staged (source opened) for verification.
    staged: Option<ComponentId>,
    /// `watching[i]`: `ComponentId(i)` is out of reset with a walk in
    /// flight. Set on `ReleaseReset`, cleared on `AssertReset` and on a
    /// terminal verdict. Only watched walks are polled, so a finished or
    /// quiesced walk emits no stale event.
    watching: [bool; N],
    /// `verified_svn[i]` is the manifest SVN of `ComponentId(i)`'s last
    /// authenticated image — the only value a floor commit may trust.
    /// `None` until a verification passes; cleared again on rejection.
    verified_svn: [Option<Svn>; N],
    /// The update job submitted by the frontend. Held until the update is
    /// activated or discarded.
    pending_update: Option<UpdateJob>,
    /// The verify session while one is running. The verifier it was
    /// started from is back in `board.update_verifier` on every terminal
    /// outcome, so exactly one of the two holds it.
    verify_session: Option<<B::UpdateVerifier as IncrementalVerifier>::Session>,
}

/// What one pump call established, before the stall rule is applied.
enum Step {
    /// The step ran and moved the job this far.
    Working(Progress),
    /// The candidate authenticated; staging is next.
    Authenticated,
    /// The device holds the complete payload.
    Staged,
    /// The candidate failed, or the device did.
    Rejected,
}

/// One in-flight update, recorded by [`PlatformDriver::submit_update`].
struct UpdateJob {
    target: ComponentId,
    /// Candidate length in bytes, from the offer the source accepted. The
    /// staging region is board geometry and usually larger, so the job
    /// carries what part of it holds this candidate.
    len: u64,
    phase: UpdatePhase,
    /// Set by the `StageUpdate` executor. The SM emits it with
    /// `AuthenticateUpdate` on entry to `Updating`, and the pump refuses
    /// to push bytes to a device the SM never asked to stage.
    stage_requested: bool,
    /// Progress at the last pump call, and when it last moved. The pump
    /// judges a stall against these; both phases count bytes the same
    /// way, so one rule covers authentication and staging.
    progress: Progress,
    /// `None` until the first pump call: the job is recorded before the
    /// event loop has a clock reading for it.
    progress_since_millis: Option<u64>,
}

/// How far the in-flight update has come.
///
/// The SM emits `AuthenticateUpdate` and `StageUpdate` together on entry
/// to `Updating` and the driver sequences them: nothing is pushed to a
/// device before the candidate authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePhase {
    /// Recorded by the frontend, no executor has run yet.
    Submitted,
    /// A verify session is running over the candidate.
    Authenticating,
    /// The candidate authenticated; `poll_stage` is pushing it to the
    /// device.
    Staging,
    /// The device holds the complete payload. The SM has been told, and
    /// the job waits for `ActivateUpdate` or `DiscardStaged`.
    Staged,
}

impl<B: BoardCapabilities, const N: usize> PlatformDriver<B, N> {
    pub fn new(board: Board<B, N>) -> Self {
        // ComponentId is a u8, so ids for N > 256 components would wrap.
        const { assert!(N <= 256) };
        Self {
            board,
            staged: None,
            watching: [false; N],
            verified_svn: [None; N],
            pending_update: None,
            verify_session: None,
        }
    }

    /// The board wiring, read-only, for the tests: they observe a
    /// capability after it moved into the driver, instead of every mock
    /// smuggling out a shared handle. Real consumers get targeted queries
    /// when they exist — not this.
    #[cfg(test)]
    pub(crate) fn board(&self) -> &Board<B, N> {
        &self.board
    }

    /// The frontend half of the update handshake: record `target` as the
    /// component the staged candidate is for and `len` as how much of the
    /// staging region it occupies. Must succeed BEFORE
    /// [`Event::UpdateRequest`] is dispatched; `StageUpdate` with no stored
    /// job fails closed. Refuses an unknown id, a candidate that does not
    /// fit the staging region, and a second submit while one update is in
    /// flight; nothing is stored on refusal, so a refused request can
    /// never surface as an update event.
    pub fn submit_update(&mut self, target: ComponentId, len: u64) -> Result<(), DriverError> {
        self.board
            .updatables
            .get(target.get() as usize)
            .ok_or(DriverError::UnknownComponent)?;
        if len > self.board.update_staging.len() {
            return Err(DriverError::CandidateOutOfRange);
        }
        if self.pending_update.is_some() {
            return Err(DriverError::UpdateBusy);
        }
        self.pending_update = Some(UpdateJob {
            target,
            len,
            phase: UpdatePhase::Submitted,
            stage_requested: false,
            progress: Progress::none(len),
            progress_since_millis: None,
        });
        Ok(())
    }

    /// Discards the in-flight update: abandons any verify session, tells
    /// the device to drop what it staged, and clears the job.
    ///
    /// Infallible on the device side ([`Updatable::abandon`] cannot fail),
    /// so the only refusal is having no job at all, which means the SM and
    /// the driver have drifted apart.
    pub fn discard_staged(&mut self) -> Result<(), DriverError> {
        let job = self.pending_update.take().ok_or(DriverError::NoUpdateJob)?;
        if let Some(session) = self.verify_session.take() {
            self.board.update_verifier = Some(session.abandon());
        }
        let updatable = self
            .board
            .updatables
            .get_mut(job.target.get() as usize)
            .ok_or(DriverError::UnknownComponent)?;
        updatable.abandon();
        Ok(())
    }

    /// Starts authenticating the candidate: takes the board's update
    /// verifier and opens a session over it. One poll happens per pump
    /// call, not here, so the executor returns promptly.
    ///
    /// The SM emits this before `StageUpdate`, so a candidate that fails
    /// authentication never reaches the device.
    pub fn authenticate_update(&mut self) -> Result<(), DriverError> {
        let job = self
            .pending_update
            .as_mut()
            .ok_or(DriverError::NoUpdateJob)?;
        if job.phase != UpdatePhase::Submitted {
            return Err(DriverError::NoUpdateJob);
        }
        // Refuse here rather than at the first poll: a candidate that does
        // not fit the region is a frontend bug, and the window is what
        // every later read goes through.
        PayloadWindow::new(&self.board.update_staging, 0, job.len)
            .map_err(|_| DriverError::CandidateOutOfRange)?;
        let verifier = self
            .board
            .update_verifier
            .take()
            .ok_or(DriverError::NoUpdateJob)?;
        self.verify_session = Some(verifier.start());
        job.phase = UpdatePhase::Authenticating;
        Ok(())
    }

    /// Records that the SM asked for staging. The transfer itself runs in
    /// [`pump_update`](Self::pump_update), which refuses to touch a
    /// device without this: the SM emits `StageUpdate` next to
    /// `AuthenticateUpdate`, and a job missing one of the pair means the
    /// two sides have drifted apart.
    pub fn stage_update(&mut self) -> Result<(), DriverError> {
        let job = self
            .pending_update
            .as_mut()
            .ok_or(DriverError::NoUpdateJob)?;
        job.stage_requested = true;
        Ok(())
    }

    /// Activates the staged candidate: the device's next boot runs it,
    /// tentatively. Clears the job, which has reached its end.
    ///
    /// The commit is not here. Activation proposes; `BootConfirmed` and
    /// `CommitSvnFloor` decide.
    pub fn activate_update(&mut self) -> Result<(), DriverError> {
        let job = self
            .pending_update
            .as_ref()
            .ok_or(DriverError::NoUpdateJob)?;
        if job.phase != UpdatePhase::Staged {
            return Err(DriverError::NoUpdateJob);
        }
        let target = job.target;
        let updatable = self
            .board
            .updatables
            .get_mut(target.get() as usize)
            .ok_or(DriverError::UnknownComponent)?;
        updatable.activate().map_err(|_| DriverError::UpdateFault)?;
        // Only now: a refused activation leaves the job in flight, so the
        // SM's DiscardStaged still finds it.
        self.pending_update = None;
        Ok(())
    }

    /// One step of the in-flight update, called by the event loop between
    /// events, as [`poll_boot_walks`](Self::poll_boot_walks) is.
    ///
    /// Authentication runs first and staging follows, both one bounded
    /// step per call, so the loop stays live through a transfer that
    /// takes minutes. A job that stops making progress for longer than
    /// the board's stall budget is abandoned here rather than waited out.
    ///
    /// The verdict travels as an event: `UpdateVerified` once the device
    /// holds the complete payload, `UpdateRejected` for a candidate that
    /// failed authentication, a device that faulted, or a stall. The
    /// event is the SM's to act on; the job stays until the SM answers
    /// with `ActivateUpdate` or `DiscardStaged`.
    pub fn pump_update(&mut self, now_millis: u64) -> UpdatePoll {
        let Some(job) = self.pending_update.as_mut() else {
            return UpdatePoll::idle();
        };
        let since = *job.progress_since_millis.get_or_insert(now_millis);
        let phase = job.phase;
        let before = job.progress;

        let stepped = match phase {
            // Nothing to pump: the executors have not run, or the device
            // already holds the payload and the SM owns the next move.
            UpdatePhase::Submitted | UpdatePhase::Staged => return UpdatePoll::idle(),
            UpdatePhase::Authenticating => self.poll_authentication(),
            UpdatePhase::Staging => self.poll_staging(),
        };

        let step = match stepped {
            Ok(step) => step,
            Err(_) => return self.reject_job(),
        };

        let job = match self.pending_update.as_mut() {
            Some(job) => job,
            None => return UpdatePoll::idle(),
        };
        match step {
            Step::Working(progress) => {
                job.progress = progress;
                if progress.done > before.done {
                    job.progress_since_millis = Some(now_millis);
                } else if now_millis.saturating_sub(since) >= self.board.update_stall_budget_millis
                {
                    return self.reject_job();
                }
                UpdatePoll {
                    event: None,
                    progress: Some(progress),
                }
            }
            // Authentication passed. Staging starts on the next call, so
            // this one stays a bounded step.
            Step::Authenticated => {
                job.phase = UpdatePhase::Staging;
                job.progress = Progress::none(job.len);
                job.progress_since_millis = Some(now_millis);
                UpdatePoll {
                    event: None,
                    progress: Some(job.progress),
                }
            }
            Step::Staged => {
                job.phase = UpdatePhase::Staged;
                UpdatePoll {
                    event: Some(Event::UpdateVerified),
                    progress: None,
                }
            }
            Step::Rejected => self.reject_job(),
        }
    }

    /// One verify poll over the candidate window. The session comes out
    /// of the driver and goes back in unless it reached a verdict.
    fn poll_authentication(&mut self) -> Result<Step, DriverError> {
        let session = self.verify_session.take().ok_or(DriverError::NoUpdateJob)?;
        let len = self
            .pending_update
            .as_ref()
            .ok_or(DriverError::NoUpdateJob)?
            .len;
        let window = PayloadWindow::new(&self.board.update_staging, 0, len)
            .map_err(|_| DriverError::CandidateOutOfRange)?;
        match session.poll(&window) {
            PollOutcome::Processing { session, progress } => {
                self.verify_session = Some(session);
                Ok(Step::Working(progress))
            }
            PollOutcome::Authenticated(verifier) => {
                self.board.update_verifier = Some(verifier);
                Ok(Step::Authenticated)
            }
            PollOutcome::Rejected(verifier) | PollOutcome::Fault(verifier, _) => {
                self.board.update_verifier = Some(verifier);
                Ok(Step::Rejected)
            }
        }
    }

    /// One staging step. Borrows the staging region and the device as
    /// separate fields so the window can be read while the device writes.
    fn poll_staging(&mut self) -> Result<Step, DriverError> {
        let job = self
            .pending_update
            .as_ref()
            .ok_or(DriverError::NoUpdateJob)?;
        if !job.stage_requested {
            return Err(DriverError::NoUpdateJob);
        }
        let (target, len) = (job.target, job.len);
        let window = PayloadWindow::new(&self.board.update_staging, 0, len)
            .map_err(|_| DriverError::CandidateOutOfRange)?;
        let updatable = self
            .board
            .updatables
            .get_mut(target.get() as usize)
            .ok_or(DriverError::UnknownComponent)?;
        match updatable.poll_stage(&window) {
            Ok(StageProgress::Transferring(progress)) => Ok(Step::Working(progress)),
            Ok(StageProgress::Ready) => Ok(Step::Staged),
            Err(_) => Ok(Step::Rejected),
        }
    }

    /// Ends the job the way the SM understands: the device drops what it
    /// staged and the verdict travels as `UpdateRejected`. The job itself
    /// stays until the SM answers with `DiscardStaged`, so the two sides
    /// never disagree about whether an update is in flight.
    fn reject_job(&mut self) -> UpdatePoll {
        if let Some(session) = self.verify_session.take() {
            self.board.update_verifier = Some(session.abandon());
        }
        if let Some(job) = self.pending_update.as_mut() {
            job.phase = UpdatePhase::Submitted;
            job.stage_requested = false;
            let target = job.target.get() as usize;
            if let Some(updatable) = self.board.updatables.get_mut(target) {
                updatable.abandon();
            }
        }
        UpdatePoll {
            event: Some(Event::UpdateRejected),
            progress: None,
        }
    }

    /// Target of the in-flight update, if one was submitted.
    pub fn pending_update(&self) -> Option<ComponentId> {
        self.pending_update.as_ref().map(|job| job.target)
    }

    /// `id`'s image source. Takes the array rather than `&mut self` so the
    /// caller can borrow `board.verifier` alongside the returned image.
    fn source(images: &mut [B::Image; N], id: ComponentId) -> Result<&mut B::Image, DriverError> {
        images
            .get_mut(id.get() as usize)
            .ok_or(DriverError::UnknownComponent)
    }

    /// Stage `id`'s image: open its source so
    /// [`verify_firmware`](Self::verify_firmware) can read it.
    pub fn stage_firmware(&mut self, id: ComponentId) -> Result<(), DriverError> {
        self.staged = None;
        let source = Self::source(&mut self.board.images, id)?;
        source.open().map_err(|_| DriverError::ImageUnavailable)?;
        self.staged = Some(id);
        Ok(())
    }

    /// Judge the staged image via the [`Verifier`] and return the verdict:
    /// `Event::VerificationPassed(id)` or `Event::VerificationFailed(id)`.
    pub fn verify_firmware(&mut self, id: ComponentId) -> Result<Event, DriverError> {
        // Id validity first: an unknown component is UnknownComponent even
        // though it can never be staged.
        let source = Self::source(&mut self.board.images, id)?;
        if self.staged != Some(id) {
            return Err(DriverError::NotStaged);
        }
        let verdict = self
            .board
            .verifier
            .verify(id, source)
            .map_err(|_| DriverError::VerifierFault)?;
        let idx = id.get() as usize;
        Ok(match verdict {
            Verdict::Authenticated { svn } => {
                self.verified_svn[idx] = Some(svn);
                Event::VerificationPassed(id)
            }
            Verdict::Rejected => {
                self.verified_svn[idx] = None;
                Event::VerificationFailed(id)
            }
        })
    }

    /// Advance `id`'s anti-rollback floor to its verified image's SVN.
    /// A self-managed component keeps its own floor; the commit is a
    /// no-op. A target at or below the current floor is the capability's
    /// documented no-op, so a replayed commit is harmless.
    pub fn commit_svn_floor(&mut self, id: ComponentId) -> Result<(), DriverError> {
        let idx = id.get() as usize;
        let SvnFloorBinding::Erot(floor) = self
            .board
            .svn_floors
            .get_mut(idx)
            .ok_or(DriverError::UnknownComponent)?
        else {
            return Ok(());
        };
        let svn = self.verified_svn[idx].ok_or(DriverError::NoVerifiedImage)?;
        floor.advance(svn).map_err(|_| DriverError::SvnFloorFault)
    }

    /// `id`'s reset actuator.
    fn boot_control(&mut self, id: ComponentId) -> Result<&mut B::BootControl, DriverError> {
        self.board
            .boot_controls
            .get_mut(id.get() as usize)
            .ok_or(DriverError::UnknownComponent)
    }

    /// Release `id` from reset and arm its boot walk;
    /// [`poll_boot_walks`](Self::poll_boot_walks) feeds the verdict back
    /// as `ComponentReady(id)`/`Booted(id)`/`BootFailed { id, .. }`. Arms on every
    /// release: a retry re-release starts a fresh walk.
    pub fn release_reset(&mut self, id: ComponentId) -> Result<(), DriverError> {
        self.boot_control(id)?
            .release()
            .map_err(|_| DriverError::BootControlFault)?;
        let idx = id.get() as usize;
        // In bounds: boot_control(id) above already rejected unknown ids.
        self.board.boot_watches[idx].arm();
        self.watching[idx] = true;
        Ok(())
    }

    /// Hold `id` in reset — a durable quiesce, not a pulse; at-rest
    /// verification and the recovery re-walk depend on it. Also stops the
    /// boot walk: a held device produces no boot signal, so polling it
    /// could only yield a stale `BootFailed`.
    pub fn assert_reset(&mut self, id: ComponentId) -> Result<(), DriverError> {
        self.boot_control(id)?
            .hold_in_reset()
            .map_err(|_| DriverError::BootControlFault)?;
        self.watching[id.get() as usize] = false;
        Ok(())
    }

    /// Polls every watched walk at `now_millis` and returns the first
    /// terminal verdict as its event: [`WalkVerdict::Complete`] becomes
    /// `ComponentReady(id)` (`Active`) or `Booted(id)` (`Passive`),
    /// [`WalkVerdict::Failed`] becomes `BootFailed { id, checkpoint, kind }`.
    /// The finished walk stops being watched;
    /// each verdict is delivered once.
    ///
    /// Returns at the first event; drain by calling until
    /// [`BootWalkPoll::event`] is `None`. Only that last poll carries a
    /// complete [`next_deadline_millis`](BootWalkPoll::next_deadline_millis)
    /// — the earliest deadline among the still-waiting walks.
    pub fn poll_boot_walks(&mut self, now_millis: u64) -> BootWalkPoll {
        let mut next_deadline_millis: Option<u64> = None;
        for idx in 0..N {
            if !self.watching[idx] {
                continue;
            }
            let id: ComponentId = (idx as u8).into();
            match self.board.boot_watches[idx].poll(now_millis) {
                WalkVerdict::Waiting { deadline_millis } => {
                    next_deadline_millis = Some(match next_deadline_millis {
                        Some(d) => d.min(deadline_millis),
                        None => deadline_millis,
                    });
                }
                WalkVerdict::Complete => {
                    self.watching[idx] = false;
                    let event = match self.board.component_kinds[idx] {
                        ComponentKind::Active => Event::ComponentReady(id),
                        ComponentKind::Passive => Event::Booted(id),
                    };
                    return BootWalkPoll {
                        event: Some(event),
                        next_deadline_millis,
                    };
                }
                WalkVerdict::Failed { checkpoint, cause } => {
                    self.watching[idx] = false;
                    let kind = match cause {
                        FailureCause::TimedOut => BootFailureKind::TimedOut,
                        FailureCause::DeviceRetriable => BootFailureKind::DeviceRetriable,
                        FailureCause::DeviceFatal => BootFailureKind::DeviceFatal,
                    };
                    return BootWalkPoll {
                        event: Some(Event::BootFailed {
                            id,
                            checkpoint,
                            kind,
                        }),
                        next_deadline_millis,
                    };
                }
            }
        }
        BootWalkPoll {
            event: None,
            next_deadline_millis,
        }
    }

    /// Restore `id`'s image from its recovery source. The verdict travels
    /// as an event, not an error: `Restored` and `SourceExhausted` are
    /// outcomes the SM handles per failure policy, while an `Err` from
    /// the mechanism is a genuine actuation fault that fails closed.
    pub fn recover_component(
        &mut self,
        id: ComponentId,
        attempt: u8,
    ) -> Result<Event, DriverError> {
        let recovery = self
            .board
            .recovery
            .get_mut(id.get() as usize)
            .ok_or(DriverError::UnknownComponent)?;
        match recovery.restore(attempt) {
            Ok(RestoreOutcome::Restored) => Ok(Event::Restored(id)),
            Ok(RestoreOutcome::SourceExhausted) => Ok(Event::RecoveryUnavailable(id)),
            Err(_) => Err(DriverError::RecoveryFault),
        }
    }

    /// Hands one report to the board's sink. Cannot fail, so reporting stays
    /// off the fail-closed path; reports arrive in the order the SM emitted
    /// them.
    pub fn report(&mut self, report: Report) {
        self.board.report_sink.report(report);
    }
}

/// One [`PlatformDriver::poll_boot_walks`] round.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UpdatePoll {
    /// The verdict, once the job reached one. `UpdateVerified` when the
    /// device holds the payload, `UpdateRejected` when the candidate, the
    /// device or the stall budget ended it.
    pub event: Option<Event>,
    /// How far the job has come, for the update source's progress
    /// report. `None` once there is nothing left to report.
    pub progress: Option<Progress>,
}

impl UpdatePoll {
    /// No job, or a job whose next move is the SM's.
    const fn idle() -> Self {
        Self {
            event: None,
            progress: None,
        }
    }
}

pub struct BootWalkPoll {
    /// The first terminal verdict's event; `None` when every watched walk
    /// is still waiting.
    pub event: Option<Event>,
    /// Earliest deadline among walks seen waiting this round. Complete only
    /// when [`event`](Self::event) is `None`: an early return skips the
    /// walks after the finished one.
    pub next_deadline_millis: Option<u64>,
}

impl<B: BoardCapabilities, const N: usize> Platform for PlatformDriver<B, N> {
    /// Routes each effect to its executor. Exhaustive: a new [`Effect`]
    /// variant must get an executor before this compiles. Synchronous
    /// results (the verification verdict) come back as the returned event;
    /// every executor error reports as [`EffectError`] — the SM treats all
    /// actuation failures the same, fail-closed.
    fn execute(&mut self, effect: Effect) -> Result<Option<Event>, EffectError> {
        match effect {
            Effect::ReadFirmware(id) => self.stage_firmware(id).map(|_| None),
            Effect::VerifyFirmware(id) => self.verify_firmware(id).map(Some),
            Effect::ReleaseReset(id) => self.release_reset(id).map(|_| None),
            Effect::AssertReset(id) => self.assert_reset(id).map(|_| None),
            Effect::CommitSvnFloor(id) => self.commit_svn_floor(id).map(|_| None),
            // Reports carry no error, so they never reach the fail-closed
            // group below.
            Effect::ReportIsolated(id) => {
                self.report(Report::Isolated(id));
                Ok(None)
            }
            Effect::ReportRecoveryFailed(id) => {
                self.report(Report::RecoveryFailed(id));
                Ok(None)
            }
            Effect::ReportUpdateDeferred => {
                self.pending_update = None;
                self.report(Report::UpdateDeferred);
                Ok(None)
            }
            Effect::ReportUpdateAborted => {
                self.pending_update = None;
                self.report(Report::UpdateAborted);
                Ok(None)
            }
            Effect::ReportBootFailed {
                id,
                checkpoint,
                kind,
            } => {
                self.report(Report::BootFailed {
                    id,
                    checkpoint,
                    kind,
                });
                Ok(None)
            }
            Effect::RecoverComponent { id, attempt } => {
                self.recover_component(id, attempt).map(Some)
            }
            Effect::AuthenticateUpdate => self.authenticate_update().map(|_| None),
            Effect::StageUpdate => self.stage_update().map(|_| None),
            Effect::ActivateUpdate => self.activate_update().map(|_| None),
            Effect::DiscardStaged => self.discard_staged().map(|_| None),
            // No board capability is composed for these seams yet, so they
            // fail closed here instead of behind stub methods.
            Effect::SignAttestation | Effect::LatchLockdown => return Err(EffectError),
            // Emit is consumed by the orchestrator; receiving one is a
            // driver bug.
            Effect::Emit(_) => return Err(EffectError),
        }
        .map_err(|_| EffectError)
    }
}

/// The connection between an update frontend and the SM: called (by the
/// event loop, on the frontend's behalf) once a complete candidate for
/// `target` sits in the staging region. Records the job first, then injects
/// [`Event::UpdateRequest`]; that order is load-bearing, `StageUpdate` can
/// never run without a target. On refusal no event is injected and the
/// frontend answers the requester over its own protocol.
pub fn request_update<B: BoardCapabilities, const N: usize, const E: usize>(
    orchestrator: &mut Orchestrator<N, E>,
    driver: &mut PlatformDriver<B, N>,
    target: ComponentId,
    len: u64,
) -> Result<(), DriverError> {
    driver.submit_update(target, len)?;
    orchestrator.dispatch(driver, Event::UpdateRequest);
    Ok(())
}
