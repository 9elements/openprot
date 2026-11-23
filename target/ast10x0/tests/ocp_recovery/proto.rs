// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! What the OCP recovery device app and the recovery flow app agree on: the
//! request and the replies on the channel between them, and the image the
//! Recovery Agent stand-in pushes.

#![no_std]

/// Request from the flow: hand over the bytes the device holds.
pub const OP_TAKE: u8 = 1;

/// Reply: the bytes of a push follow the kind byte.
pub const REPLY_BYTES: u8 = 1;
/// Reply: the device handed over every byte of the announced image.
pub const REPLY_IMAGE_COMPLETE: u8 = 2;

/// The kind byte that opens every reply.
pub const REPLY_HEADER_LEN: usize = 1;

/// The largest push, which is also what the slot in the device holds.
pub const MAX_TRANSFER: usize = 252;

/// The length of the flash region the image lands in. The device refuses an
/// announced size above `REGION_LEN`, and the flow checks that the region fits
/// in the flash.
pub const REGION_LEN: usize = 2 * SECTOR_LEN;

/// The sector size the image is laid out for. The flow fails on a flash
/// with another sector size.
pub const SECTOR_LEN: usize = 4096;

/// How far the image runs into the second sector.
const TAIL_LEN: usize = 1000;

/// One sector plus a tail, so the flow erases a second sector in the middle
/// of the push, and the last push is shorter than [`MAX_TRANSFER`].
pub const IMAGE_LEN: usize = SECTOR_LEN + TAIL_LEN;

/// INDIRECT_FIFO_CTRL announces the image size in units of this many bytes.
const DWORD: usize = 4;

const _: () = {
    assert!(IMAGE_LEN % DWORD == 0);
    assert!(IMAGE_LEN % MAX_TRANSFER != 0);
    assert!(IMAGE_LEN <= REGION_LEN);
};

/// Byte `i` of the image.
pub fn pattern(i: usize) -> u8 {
    (i as u8).wrapping_mul(31).wrapping_add(7)
}
