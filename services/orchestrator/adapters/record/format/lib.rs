// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The bytes the eRoT and its boot stage share for a self-update.
//!
//! The boot stage (ROM or bootloader) starts one image slot per boot. The
//! eRoT writes a [`Record`] that steers that choice: which slot is
//! confirmed, and the session of an update in flight. The boot stage reads
//! the record, picks a slot with [`select`], checks the signature and the
//! SVN floor of that slot, and hands the started slot and its SVN to the
//! image as a [`Handover`].
//!
//! The crate has no dependencies and no I/O, so a boot stage outside the
//! orchestrator links it too, and the record layout has one definition.
//!
//! On storage that cannot write a record atomically, the eRoT keeps copies
//! A and B, writes the copy that does not hold the newest record, and reads
//! the newest copy that decodes ([`newest`]). The eRoT writes the record,
//! and the boot stage only reads it.

#![cfg_attr(not(test), no_std)]

mod crc32;
mod mark;
mod record;
mod select;

pub use crc32::crc32;
pub use mark::{Handover, TriedMark, MARK_LEN};
pub use record::{newest, CopyId, DecodeError, Phase, Record, Session, RECORD_LEN, RECORD_VERSION};
pub use select::{pick_trial_slot, select, Start};
