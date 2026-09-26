// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! How far a polled step has come.

/// Bytes processed so far out of the total.
///
/// Shared by every polled seam that works through a payload:
/// [`StageProgress::Transferring`](crate::StageProgress::Transferring)
/// counts bytes written to a device,
/// [`PollOutcome::Processing`](crate::PollOutcome::Processing) counts bytes
/// hashed. `done` is what a caller watching for a stall keys on, so the
/// two seams answer the same question the same way.
///
/// `done` is monotonic within one job and may hold still across calls: a
/// busy device or a retransmit makes no progress and is not an error. It
/// never exceeds `total`.
///
/// The update intake seam has its own `Progress` in
/// `orchestrator-update-api` with a `written` field. That one is a wire
/// form the PLDM service decodes, and this crate stays out of that
/// process, so the two types are counterparts rather than one shared
/// definition. The driver converts when it answers an intake poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes processed so far, at or below `total`.
    pub done: u64,
    /// Total payload bytes.
    pub total: u64,
}

impl Progress {
    /// A job that has processed nothing yet.
    pub const fn none(total: u64) -> Self {
        Self { done: 0, total }
    }

    /// True once every byte has been processed.
    pub const fn is_complete(&self) -> bool {
        self.done >= self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_job_has_processed_nothing() {
        let p = Progress::none(4096);
        assert_eq!(p.done, 0);
        assert_eq!(p.total, 4096);
        assert!(!p.is_complete());
    }

    #[test]
    fn a_job_is_complete_when_done_reaches_total() {
        assert!(Progress {
            done: 10,
            total: 10
        }
        .is_complete());
        assert!(!Progress { done: 9, total: 10 }.is_complete());
    }

    #[test]
    fn an_empty_payload_is_complete_from_the_start() {
        assert!(Progress::none(0).is_complete());
    }
}
