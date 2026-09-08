// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The [`IncrementalVerifier`] update-verification capability contract.

use crate::PayloadSource;

/// Incremental firmware verification: judge a candidate image one bounded
/// step at a time, so a single-threaded runtime stays live while hashing
/// megabytes of payload.
///
/// The caller (the platform driver's update pump) calls [`start`] once to
/// begin a session, then calls [`poll`] repeatedly on its own clock. Each
/// poll does at most one read from the payload and one hash update, then
/// returns. The chunk size is implementor-chosen, sized so each poll fits
/// the caller's per-poll time budget. The caller watches `hashed`
/// progress and abandons a session that stalls, on its own budget; the
/// verifier never judges liveness.
///
/// A session produces exactly one terminal verdict: [`Authenticated`] or
/// [`Rejected`]. A fault (unreadable payload, crypto engine error) is
/// `Err`, not a verdict, because a check that could not run must not forge
/// a judgment. After an `Err`, the session is abandoned; only [`start`]
/// returns the verifier to a defined state.
///
/// [`start`] discards any in-progress session, so the caller can abandon
/// and restart without a separate reset method. Unlike `Updatable`, which
/// starts implicitly from idle, the explicit `start` ensures that one
/// extra poll after a verdict is caught rather than silently re-hashing
/// from zero. The boot-time synchronous `Verifier` (in the driver crate)
/// is unaffected: it stays one-shot for the chain walk, where the image is
/// small and local.
///
/// [`start`]: IncrementalVerifier::start
/// [`poll`]: IncrementalVerifier::poll
/// [`Authenticated`]: VerifyStep::Authenticated
/// [`Rejected`]: VerifyStep::Rejected
pub trait IncrementalVerifier {
    /// The error reported when the check itself cannot run: crypto fault,
    /// unreadable payload, or misuse (poll outside a session). A bad image
    /// is [`Rejected`](VerifyStep::Rejected), not an error.
    type Error: core::error::Error;

    /// Discards any in-progress session and prepares to verify from the
    /// start of the image. The next [`poll`](Self::poll) begins hashing
    /// at offset zero.
    fn start(&mut self);

    /// Processes one bounded step: at most one read from `payload` and one
    /// hash update, then returns. Never waits on device progress, never
    /// sleeps. Returns the session's current state: still processing
    /// (with byte-level progress), or a terminal verdict.
    ///
    /// Calling `poll` after a terminal verdict (or before [`start`](Self::start))
    /// is an error.
    fn poll(&mut self, payload: &dyn PayloadSource) -> Result<VerifyStep, Self::Error>;
}

/// One step of an incremental verification session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStep {
    /// One chunk hashed. `hashed` bytes processed so far out of `total`.
    /// The caller watches progress and abandons a session whose `hashed`
    /// stops advancing, on its own stall budget.
    Processing { hashed: u64, total: u64 },
    /// The complete image authenticated (signature and policy checks passed).
    Authenticated,
    /// The complete image was checked and found invalid.
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PayloadReadError, PayloadSource};

    // A PayloadSource over a plain byte slice.
    struct SlicePayload(&'static [u8]);

    impl PayloadSource for SlicePayload {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), PayloadReadError> {
            let start = usize::try_from(offset).map_err(|_| PayloadReadError::OutOfRange)?;
            let end = start
                .checked_add(buf.len())
                .ok_or(PayloadReadError::OutOfRange)?;
            buf.copy_from_slice(self.0.get(start..end).ok_or(PayloadReadError::OutOfRange)?);
            Ok(())
        }
    }

    // A verifier that hashes 4 bytes per poll and accepts any image whose
    // first byte is nonzero.
    struct ChunkedVerifier {
        offset: u64,
        total: u64,
        active: bool,
    }

    #[derive(Debug, PartialEq)]
    struct VerifierFault;

    impl core::fmt::Display for VerifierFault {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("verifier fault")
        }
    }

    impl core::error::Error for VerifierFault {}

    impl ChunkedVerifier {
        fn new() -> Self {
            Self {
                offset: 0,
                total: 0,
                active: false,
            }
        }
    }

    impl IncrementalVerifier for ChunkedVerifier {
        type Error = VerifierFault;

        fn start(&mut self) {
            self.offset = 0;
            self.total = 0;
            self.active = true;
        }

        fn poll(&mut self, payload: &dyn PayloadSource) -> Result<VerifyStep, VerifierFault> {
            if !self.active {
                return Err(VerifierFault);
            }
            if self.total == 0 {
                self.total = payload.len();
            }
            if self.offset >= self.total {
                self.active = false;
                let mut first = [0u8; 1];
                payload.read_at(0, &mut first).map_err(|_| VerifierFault)?;
                return Ok(if first[0] != 0 {
                    VerifyStep::Authenticated
                } else {
                    VerifyStep::Rejected
                });
            }
            let chunk = core::cmp::min(4, (self.total - self.offset) as usize);
            let mut buf = [0u8; 4];
            payload
                .read_at(self.offset, &mut buf[..chunk])
                .map_err(|_| VerifierFault)?;
            self.offset += chunk as u64;
            Ok(VerifyStep::Processing {
                hashed: self.offset,
                total: self.total,
            })
        }
    }

    #[test]
    fn multi_poll_until_authenticated() {
        let payload = SlicePayload(&[0xAA; 10]);
        let mut v = ChunkedVerifier::new();
        v.start();

        // 4 bytes, 4 bytes, 2 bytes = 3 Processing steps, then verdict.
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 4,
                total: 10
            })
        );
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 8,
                total: 10
            })
        );
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 10,
                total: 10
            })
        );
        assert_eq!(v.poll(&payload), Ok(VerifyStep::Authenticated));
    }

    #[test]
    fn rejected_image() {
        let payload = SlicePayload(&[0x00; 8]);
        let mut v = ChunkedVerifier::new();
        v.start();

        // Drain processing steps.
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 4,
                total: 8
            })
        );
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 8,
                total: 8
            })
        );
        assert_eq!(v.poll(&payload), Ok(VerifyStep::Rejected));
    }

    #[test]
    fn start_discards_in_progress_session() {
        let payload = SlicePayload(&[0xFF; 12]);
        let mut v = ChunkedVerifier::new();
        v.start();

        // Partial progress.
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 4,
                total: 12
            })
        );

        // Restart: offset resets, next poll begins from zero.
        v.start();
        assert_eq!(
            v.poll(&payload),
            Ok(VerifyStep::Processing {
                hashed: 4,
                total: 12
            })
        );
    }

    #[test]
    fn poll_before_start_is_an_error() {
        let payload = SlicePayload(&[0xFF; 4]);
        let mut v = ChunkedVerifier::new();
        assert!(v.poll(&payload).is_err());
    }

    // A PayloadSource whose read_at always fails.
    struct Lying;

    impl PayloadSource for Lying {
        fn len(&self) -> u64 {
            64
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<(), PayloadReadError> {
            Err(PayloadReadError::Storage)
        }
    }

    #[test]
    fn read_fault_is_err_not_verdict() {
        let mut v = ChunkedVerifier::new();
        v.start();

        // The verifier tries to read, fails, and returns Err (not Rejected).
        let result = v.poll(&Lying);
        assert!(result.is_err(), "payload fault must be Err, not a verdict");
    }

    #[test]
    fn poll_after_verdict_is_an_error() {
        let payload = SlicePayload(&[0xFF; 4]);
        let mut v = ChunkedVerifier::new();
        v.start();

        // Drain to verdict.
        let _ = v.poll(&payload);
        let _ = v.poll(&payload);

        // Post-verdict poll is a fault.
        assert!(v.poll(&payload).is_err());
    }
}
