// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The Recovery Agent for 1 downstream device, for a task outside the
//! orchestrator loop.
//!
//! [`FifoPush`] is 1 sequence. [`RecoveryAgent`] owns the register access
//! and the chunk buffer, borrows the image source, and decides when a
//! sequence runs: only in a boot cycle the orchestrator side released, and
//! at most 1 recovery per boot cycle. Outside a released boot cycle a
//! request of the downstream device for an image gets no push.
//!
//! The orchestrator side and the task exchange plain values. A
//! [`BootCyclePost`] tells the task that the reset line of the downstream
//! device moved. A [`PushReport`] tells the orchestrator side how the push
//! stands, tagged with the boot cycle number of the post. Neither side
//! waits for the other.
//!
//! The task that hosts a `RecoveryAgent` follows these rules:
//!
//! - The task hands the newest post to [`RecoveryAgent::post`] between
//!   register accesses.
//! - The task sends every report [`RecoveryAgent::take_report`] returns,
//!   and the report wakes the orchestrator loop. The boot watch of the
//!   orchestrator reads evidence only when polled, and judges a lapsed
//!   window as a timeout before any read.
//! - The task waits its poll period after [`Served::Waiting`] and after
//!   [`Served::Retrying`], and runs no stall timer: the checkpoint timeout
//!   of the orchestrator ends a stalled boot attempt. [`PushLimits`] bounds
//!   the images and the restarts of 1 boot cycle. A step that fails and
//!   repeats is outside both limits, so the poll period sets the rate of
//!   the repeated accesses.

use crate::{AgentError, FifoPush, ImageSource, Outcome, Phase, Progress, RegisterAccess};
use openprot_ocp_recovery_api::regs::ImageSelection;

/// What the orchestrator side posts to the task when the reset line of the
/// downstream device moved.
///
/// A newer post replaces a post the task has not taken yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCyclePost {
    /// The orchestrator holds the downstream device in reset.
    Held,
    /// The orchestrator released the downstream device, which begins the
    /// boot cycle `cycle`.
    Released {
        /// The number the orchestrator side gave the boot cycle.
        cycle: u32,
    },
}

/// How the push stands in the boot cycle `cycle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushReport {
    /// The boot cycle number of the last [`BootCyclePost::Released`].
    pub cycle: u32,
    /// The status in that boot cycle.
    pub status: PushStatus,
}

/// What the push recorded since the last post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushStatus {
    /// The downstream device reported no end state yet. The status of a
    /// held downstream device is `NoEndState` with 0 activations.
    NoEndState {
        /// How many images the downstream device activated in the boot
        /// cycle.
        activated: u32,
    },
    /// The downstream device reported an end state.
    Ended(Outcome),
    /// The downstream device requests an image the image source does not
    /// hold.
    ImageNotHeld {
        /// The image index the downstream device requests.
        index: u8,
    },
    /// The FIFO registers cannot state the size of the requested image.
    ImageSize,
    /// The image source failed a read. The agent repeats the read.
    ImageRead,
    /// The downstream device requests more images than
    /// [`PushLimits::max_images`].
    ImageLimit,
    /// The sequence stopped more often than [`PushLimits::max_restarts`].
    RestartLimit,
}

/// What the board allows in 1 boot cycle.
///
/// On an orchestrator without an executor for a recovery, a boot attempt
/// that ends in a timeout brings no `Held` post, so the agent stays
/// active. The limits bound the images and the restarts of the boot cycle.
/// A step that fails and repeats is outside both limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushLimits {
    /// How many images the downstream device may activate.
    pub max_images: u32,
    /// How many times the agent starts the sequence again after a stop.
    pub max_restarts: u32,
}

/// What a [`RecoveryAgent::poll`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served<E> {
    /// No released boot cycle, or the recovery of the boot cycle is over.
    /// The poll made no register access.
    Idle,
    /// The access went through. The caller polls again at once.
    Advanced,
    /// The downstream device or the image source is not ready, as
    /// [`Progress::Waiting`] says for the sequence.
    Waiting,
    /// The downstream device reported an end state. The agent is idle.
    Ended(Outcome),
    /// The sequence is over. After a refused write, an announcement
    /// mismatch or a failed chunk write, the next poll starts a new sequence
    /// while [`PushLimits::max_restarts`] allows. After an image error, or
    /// at the limit, the agent is idle and the status names the reason.
    Stopped(AgentError<E>),
    /// The step failed, and the next poll makes the same step again.
    Retrying(AgentError<E>),
}

/// What the agent does after an error of [`FifoPush::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterError {
    /// Keep the sequence, so the next poll makes the same step again.
    Retry,
    /// Drop the sequence and start a new sequence, while
    /// [`PushLimits::max_restarts`] allows.
    Restart,
    /// End the recovery of the boot cycle.
    Idle,
}

/// What `error` of a step in `phase` leads to, and the status to record.
///
/// A failed access in the chunk write may have arrived, so the agent
/// starts the sequence again: the new announcement resets the FIFO, so the
/// downstream device stages every chunk once.
fn after_error<E>(phase: Phase, error: &AgentError<E>) -> (AfterError, Option<PushStatus>) {
    match error {
        AgentError::Access(_) if phase == Phase::WriteChunk => (AfterError::Restart, None),
        AgentError::Access(_) => (AfterError::Retry, None),
        AgentError::Protocol { .. } => (AfterError::Restart, None),
        AgentError::AnnouncementMismatch { .. } => (AfterError::Restart, None),
        AgentError::Malformed(_) => (AfterError::Retry, None),
        AgentError::MissingCapability(_) => (AfterError::Retry, None),
        AgentError::NoChunkSpace => (AfterError::Retry, None),
        AgentError::ImageRead(_) => (AfterError::Retry, Some(PushStatus::ImageRead)),
        AgentError::ImageNotHeld(index) => (
            AfterError::Idle,
            Some(PushStatus::ImageNotHeld { index: index.get() }),
        ),
        AgentError::ImageSize(_) => (AfterError::Idle, Some(PushStatus::ImageSize)),
        AgentError::ImageLimit(_) => (AfterError::Idle, Some(PushStatus::ImageLimit)),
    }
}

/// The smallest `CHUNK` in bytes. The FIFO takes an image in units of 4
/// bytes, so a shorter buffer holds no chunk.
pub const MIN_CHUNK_BYTES: usize = 4;

/// The Recovery Agent for the downstream device behind `R`.
///
/// `CHUNK` is the size in bytes of the buffer that holds a chunk between
/// the image read and the FIFO write. The build fails for a `CHUNK` below
/// [`MIN_CHUNK_BYTES`].
pub struct RecoveryAgent<'a, R, const CHUNK: usize> {
    regs: R,
    source: &'a dyn ImageSource,
    cms: u8,
    selection: ImageSelection,
    limits: PushLimits,
    push: FifoPush<'a>,
    scratch: [u8; CHUNK],
    /// A sequence runs. `false` before the first `Released` post, after a
    /// `Held` post and once the recovery of the boot cycle is over.
    active: bool,
    /// The boot cycle of the last `Released` post.
    cycle: Option<u32>,
    /// Restarts in the boot cycle.
    restarts: u32,
    /// Images the downstream device activated in sequences that a restart
    /// ended in the boot cycle. They count against
    /// [`PushLimits::max_images`].
    dropped_activations: u32,
    status: PushStatus,
    /// The status of the last report, or of the last post.
    reported: PushStatus,
}

impl<'a, R: RegisterAccess, const CHUNK: usize> RecoveryAgent<'a, R, CHUNK> {
    /// Fails the build for a `CHUNK` below [`MIN_CHUNK_BYTES`]. The compiler
    /// evaluates the assert only where code names the constant, so `new`
    /// names the constant.
    const CHUNK_IS_LONG_ENOUGH: () = assert!(CHUNK >= MIN_CHUNK_BYTES);

    /// The status after a post, which is no report.
    const AFTER_POST: PushStatus = PushStatus::NoEndState { activated: 0 };

    /// Prepares the Recovery Agent for the downstream device behind `regs`,
    /// with the images of `source` for the component memory space `cms`.
    ///
    /// `selection` is byte 1 of each `RECOVERY_CTRL` write that activates an
    /// image. The agent is idle until the first
    /// [`BootCyclePost::Released`].
    pub fn new(
        regs: R,
        source: &'a dyn ImageSource,
        cms: u8,
        selection: ImageSelection,
        limits: PushLimits,
    ) -> Self {
        let () = Self::CHUNK_IS_LONG_ENOUGH;
        Self {
            regs,
            source,
            cms,
            selection,
            limits,
            push: FifoPush::with_source(source, cms, selection),
            scratch: [0; CHUNK],
            active: false,
            cycle: None,
            restarts: 0,
            dropped_activations: 0,
            status: Self::AFTER_POST,
            reported: Self::AFTER_POST,
        }
    }

    /// Takes a post of the orchestrator side, without a register access.
    ///
    /// `Released` starts a fresh sequence for the new boot cycle, also in
    /// the middle of a push, so the next poll reads `PROT_CAP`. `Held`
    /// drops the sequence and makes the agent idle. A post sets the status
    /// that [`take_report`](Self::take_report) compares against, so
    /// `take_report` returns no report until a poll changes the status.
    pub fn post(&mut self, post: BootCyclePost) {
        self.restarts = 0;
        self.dropped_activations = 0;
        self.status = Self::AFTER_POST;
        self.reported = Self::AFTER_POST;
        self.push = self.fresh_sequence();
        match post {
            BootCyclePost::Held => self.active = false,
            BootCyclePost::Released { cycle } => {
                self.cycle = Some(cycle);
                self.active = true;
            }
        }
    }

    /// Makes at most 1 register access. An idle agent makes none.
    pub fn poll(&mut self) -> Served<R::Error> {
        if !self.active {
            return Served::Idle;
        }
        let phase = self.push.phase();
        let error = match self.push.step(&mut self.regs, &mut self.scratch) {
            Ok(Progress::Continue | Progress::ChunkWritten(_)) => {
                self.status = self.no_end_state();
                return Served::Advanced;
            }
            Ok(Progress::Waiting) => {
                self.status = self.no_end_state();
                return Served::Waiting;
            }
            Ok(Progress::Done(outcome)) => {
                self.status = PushStatus::Ended(outcome);
                self.active = false;
                return Served::Ended(outcome);
            }
            Err(error) => error,
        };
        let (then, status) = after_error(phase, &error);
        if let Some(status) = status {
            self.status = status;
        }
        match then {
            AfterError::Retry => Served::Retrying(error),
            AfterError::Idle => {
                self.active = false;
                Served::Stopped(error)
            }
            AfterError::Restart => {
                if self.restarts < self.limits.max_restarts {
                    self.restarts += 1;
                    self.dropped_activations = self
                        .dropped_activations
                        .saturating_add(self.push.activated_images());
                    self.push = self.fresh_sequence();
                } else {
                    self.status = PushStatus::RestartLimit;
                    self.active = false;
                }
                Served::Stopped(error)
            }
        }
    }

    /// The report to send: `Some` once after each change of the status that
    /// a poll made, with the boot cycle number of the last
    /// [`BootCyclePost::Released`].
    pub fn take_report(&mut self) -> Option<PushReport> {
        let cycle = self.cycle?;
        if self.status == self.reported {
            return None;
        }
        self.reported = self.status;
        Some(PushReport {
            cycle,
            status: self.status,
        })
    }

    /// The running sequence, for its phase and its counts.
    pub fn sequence(&self) -> &FifoPush<'a> {
        &self.push
    }

    /// A sequence from `PROT_CAP` on, with the images the boot cycle still
    /// allows.
    fn fresh_sequence(&self) -> FifoPush<'a> {
        let allowed = self
            .limits
            .max_images
            .saturating_sub(self.dropped_activations);
        FifoPush::with_source(self.source, self.cms, self.selection).with_image_limit(allowed)
    }

    fn no_end_state(&self) -> PushStatus {
        PushStatus::NoEndState {
            activated: self
                .dropped_activations
                .saturating_add(self.push.activated_images()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageSizeError;
    use openprot_ocp_recovery_api::regs::ImageIndex;
    use openprot_ocp_recovery_api::RecoveryCommand;
    use util_io::ByteReadError;

    // `after_error` reads the payload of 1 error only, the image index of a
    // missing image, so the other payloads below are arbitrary.

    /// An access error of a bus the tests do not model.
    #[derive(Debug, PartialEq)]
    struct BusFault;

    const COMMAND: RecoveryCommand = RecoveryCommand::IndirectFifoCtrl;

    #[test]
    fn a_failed_chunk_write_restarts_the_sequence() {
        assert_eq!(
            after_error(Phase::WriteChunk, &AgentError::Access(BusFault)),
            (AfterError::Restart, None)
        );
    }

    #[test]
    fn a_failed_access_outside_the_chunk_write_repeats_the_step() {
        assert_eq!(
            after_error(Phase::ReadFifoStatus, &AgentError::Access(BusFault)),
            (AfterError::Retry, None)
        );
    }

    #[test]
    fn a_refused_write_and_a_wrong_announcement_restart_the_sequence() {
        let errors: [AgentError<BusFault>; 2] = [
            AgentError::Protocol {
                command: COMMAND,
                code: 1,
            },
            AgentError::AnnouncementMismatch {
                announced: 8,
                read_back: 4,
            },
        ];
        for error in errors {
            assert_eq!(
                after_error(Phase::ConfirmWrite, &error),
                (AfterError::Restart, None)
            );
        }
    }

    #[test]
    fn an_answer_the_agent_cannot_use_repeats_the_step() {
        let errors: [AgentError<BusFault>; 3] = [
            AgentError::Malformed(COMMAND),
            AgentError::MissingCapability(1),
            AgentError::NoChunkSpace,
        ];
        for error in errors {
            assert_eq!(
                after_error(Phase::ReadProtCap, &error),
                (AfterError::Retry, None)
            );
        }
    }

    #[test]
    fn a_failed_image_read_repeats_the_step_and_records_the_read() {
        let error: AgentError<BusFault> = AgentError::ImageRead(ByteReadError::Storage);
        assert_eq!(
            after_error(Phase::WriteChunk, &error),
            (AfterError::Retry, Some(PushStatus::ImageRead))
        );
    }

    #[test]
    fn an_image_the_agent_cannot_push_ends_the_recovery_of_the_boot_cycle() {
        let index = ImageIndex::FIRST;
        let cases: [(AgentError<BusFault>, PushStatus); 3] = [
            (
                AgentError::ImageNotHeld(index),
                PushStatus::ImageNotHeld { index: index.get() },
            ),
            (
                AgentError::ImageSize(ImageSizeError(0)),
                PushStatus::ImageSize,
            ),
            (AgentError::ImageLimit(index), PushStatus::ImageLimit),
        ];
        for (error, status) in cases {
            assert_eq!(
                after_error(Phase::ReadRecoveryStatus, &error),
                (AfterError::Idle, Some(status))
            );
        }
    }
}
