// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! A bounded view over part of a [`PayloadSource`].

use crate::{PayloadReadError, PayloadSource};

/// An offset-and-length view over another [`PayloadSource`].
///
/// The staging region holds one candidate at a time, but the region is
/// board geometry and the candidate is only as long as the offer said. A
/// window pins the job's bytes so the reader cannot run past them: `len`
/// is what [`PayloadSource::len`] reports, and reads are relative to the
/// window, so an adapter pulling at offset 0 gets the first byte of the
/// candidate rather than the first byte of the region.
///
/// Reads outside the window are
/// [`OutOfRange`](PayloadReadError::OutOfRange), the same answer the
/// underlying source gives for a read past its end. A window that does
/// not fit its source is refused at construction instead, because a
/// caller that cannot describe the candidate has a bug the first read
/// would only hide.
pub struct PayloadWindow<'a, S: PayloadSource + ?Sized> {
    source: &'a S,
    offset: u64,
    len: u64,
}

impl<'a, S: PayloadSource + ?Sized> PayloadWindow<'a, S> {
    /// Views `len` bytes of `source` starting at `offset`.
    ///
    /// Returns [`OutOfRange`](PayloadReadError::OutOfRange) if the window
    /// runs past the end of `source`.
    pub fn new(source: &'a S, offset: u64, len: u64) -> Result<Self, PayloadReadError> {
        let end = offset
            .checked_add(len)
            .ok_or(PayloadReadError::OutOfRange)?;
        if end > source.len() {
            return Err(PayloadReadError::OutOfRange);
        }
        Ok(Self {
            source,
            offset,
            len,
        })
    }
}

impl<S: PayloadSource + ?Sized> PayloadSource for PayloadWindow<'_, S> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), PayloadReadError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(PayloadReadError::OutOfRange)?;
        if end > self.len {
            return Err(PayloadReadError::OutOfRange);
        }
        self.source.read_at(self.offset + offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SliceSource(&'static [u8]);

    impl PayloadSource for SliceSource {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), PayloadReadError> {
            let start = offset as usize;
            let end = start
                .checked_add(buf.len())
                .ok_or(PayloadReadError::OutOfRange)?;
            if end > self.0.len() {
                return Err(PayloadReadError::OutOfRange);
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    const REGION: SliceSource = SliceSource(&[0, 1, 2, 3, 4, 5, 6, 7]);

    #[test]
    fn the_window_reports_its_own_length_not_the_regions() {
        let w = PayloadWindow::new(&REGION, 2, 3).unwrap();
        assert_eq!(w.len(), 3);
        assert!(!w.is_empty());
    }

    #[test]
    fn a_read_at_zero_starts_at_the_window_offset() {
        let w = PayloadWindow::new(&REGION, 2, 3).unwrap();
        let mut buf = [0u8; 3];
        w.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [2, 3, 4]);
    }

    #[test]
    fn a_read_past_the_window_end_is_out_of_range() {
        let w = PayloadWindow::new(&REGION, 2, 3).unwrap();
        let mut buf = [0u8; 2];
        // Bytes 5 and 6 exist in the region but not in the window.
        assert_eq!(w.read_at(2, &mut buf), Err(PayloadReadError::OutOfRange));
    }

    #[test]
    fn a_window_past_the_end_of_the_source_is_refused() {
        assert_eq!(
            PayloadWindow::new(&REGION, 6, 4).err(),
            Some(PayloadReadError::OutOfRange)
        );
    }

    #[test]
    fn a_window_offset_that_overflows_is_refused() {
        assert_eq!(
            PayloadWindow::new(&REGION, u64::MAX, 1).err(),
            Some(PayloadReadError::OutOfRange)
        );
    }

    #[test]
    fn an_empty_window_is_empty() {
        let w = PayloadWindow::new(&REGION, 8, 0).unwrap();
        assert!(w.is_empty());
    }
}
