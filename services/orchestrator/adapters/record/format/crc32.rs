// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! CRC-32 (IEEE 802.3, reflected), bit by bit.
//!
//! The record and the mark are a few bytes each, so the bitwise loop runs
//! fast enough and saves the flash a lookup table takes. The boot stage and
//! the eRoT link this function, so both compute the same checksum.

/// The reflected IEEE 802.3 polynomial.
const POLY: u32 = 0xEDB8_8320;

/// CRC-32 of `bytes`, as zlib and Ethernet compute it.
#[must_use]
pub const fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    let mut i = 0;
    while i < bytes.len() {
        crc ^= bytes[i] as u32;
        let mut bit = 0;
        while bit < u8::BITS {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        i += 1;
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // The check value every CRC-32/ISO-HDLC implementation publishes.
    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_gives_zero() {
        assert_eq!(crc32(&[]), 0);
    }
}
