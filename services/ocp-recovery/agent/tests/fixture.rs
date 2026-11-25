// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The device behind `dispatch` and the images, shared by the host tests of
//! the Recovery Agent crate.

use openprot_ocp_recovery_agent::{ImageLookup, ImageSource, RegisterAccess};
use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, ImageIndex, IndirectFifoStatus, ProtCap, RecoveryReason, RecoveryState,
};
use openprot_ocp_recovery_api::RecoveryCommand;
use openprot_ocp_recovery_server::{dispatch, RamStaging, RecoveryInterface};
use std::cell::Cell;
use util_io::{ByteReadError, ByteSource};

/// Largest transfer the device reports in `INDIRECT_FIFO_STATUS`.
pub const DEVICE_MAX_TRANSFER: usize = 64;

/// The FIFO registers count 4-byte units.
pub const DWORD: usize = 4;

/// Chunks of the largest transfer in the image.
pub const FULL_CHUNKS: usize = 3;

/// Length of the last chunk, shorter than the largest transfer.
pub const LAST_CHUNK_BYTES: usize = 20;

/// Size of the image the agent pushes.
pub const IMAGE_BYTES: usize = FULL_CHUNKS * DEVICE_MAX_TRANSFER + LAST_CHUNK_BYTES;

/// Period of the image pattern. A prime, so no chunk size divides it and a
/// chunk at the wrong offset holds other bytes.
pub const PATTERN_PERIOD: usize = 251;

/// Capacity of the staging region.
pub const STAGING_BYTES: usize = 512;

pub const DEVICE_ID: &[u8] = &[0xa5; 24];

/// The component memory space of the code region.
pub const CODE_CMS: u8 = 0;

/// Upper bound on the steps of a test, far above what a push needs. A
/// sequence that never waits, ends or returns an error panics in
/// `run_until_it_stops` at this count.
pub const STEP_LIMIT: usize = 1000;

pub const STREAMING_CAPABILITIES: u16 = capability::IDENTIFICATION
    | capability::DEVICE_STATUS
    | capability::PUSH_C_IMAGE
    | capability::FLASHLESS_BOOT
    | capability::FIFO_CMS;

/// An image in RAM.
pub struct SliceImage(pub Vec<u8>);

impl SliceImage {
    /// A pattern that shows a chunk the device stored at the wrong offset.
    pub fn pattern(len: usize) -> Self {
        Self((0..len).map(|i| (i % PATTERN_PERIOD) as u8).collect())
    }
}

impl ByteSource for SliceImage {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ByteReadError> {
        let start = usize::try_from(offset).map_err(|_| ByteReadError::OutOfRange)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(ByteReadError::OutOfRange)?;
        let bytes = self.0.get(start..end).ok_or(ByteReadError::OutOfRange)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

/// An image whose storage fails every read.
pub struct BrokenImage;

impl ByteSource for BrokenImage {
    fn len(&self) -> u64 {
        IMAGE_BYTES as u64
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<(), ByteReadError> {
        Err(ByteReadError::Storage)
    }
}

/// The access did not complete, as on a bus that lost the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusFault;

/// What the test changes in the answer to a read, to produce answers
/// `dispatch` never gives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    /// The access fails.
    Bus,
    /// The answer is 1 byte short.
    Short,
    /// `INDIRECT_FIFO_STATUS` reports the FIFO not empty.
    FifoNotEmpty,
    /// `INDIRECT_FIFO_CTRL` reads back an image size 1 unit too large.
    OtherImageSize,
}

/// The device behind `dispatch`, which `Device` calls as a transport server
/// would, plus a record of every access the agent made.
pub struct Device {
    pub iface: RecoveryInterface,
    pub staging: RamStaging<STAGING_BYTES>,
    /// Largest payload `dispatch` accepts per access.
    pub transport_max: usize,
    pub writes: Vec<(RecoveryCommand, Vec<u8>)>,
    pub reads: usize,
    /// The fault the next reads of a command carry.
    pub read_fault: Option<(RecoveryCommand, ReadFault)>,
}

impl Device {
    pub fn with_capabilities(capabilities: u16) -> Self {
        let prot_cap = ProtCap::new(capabilities, 1, 0, 0);
        let fifo_status =
            IndirectFifoStatus::code_region(STAGING_BYTES as u32, DEVICE_MAX_TRANSFER as u32)
                .expect("the region and the transfer cap divide by 4");
        Self {
            iface: RecoveryInterface::new(prot_cap, DEVICE_ID, fifo_status),
            staging: RamStaging::default(),
            transport_max: DEVICE_MAX_TRANSFER,
            writes: Vec::new(),
            reads: 0,
            read_fault: None,
        }
    }

    /// A device in recovery mode that waits for an image.
    pub fn awaiting_an_image() -> Self {
        let mut device = Self::with_capabilities(STREAMING_CAPABILITIES);
        device.iface.set_device_state(
            DeviceState::RecoveryModeReady,
            RecoveryReason::FlashlessStreamingBoot,
        );
        device
            .iface
            .set_recovery_status(RecoveryState::AwaitingImage, ImageIndex::FIRST);
        device
    }

    pub fn writes_to(&self, command: RecoveryCommand) -> Vec<&[u8]> {
        self.writes
            .iter()
            .filter(|(written, _)| *written == command)
            .map(|(_, bytes)| bytes.as_slice())
            .collect()
    }
}

impl RegisterAccess for Device {
    type Error = BusFault;

    fn write(&mut self, command: RecoveryCommand, bytes: &[u8]) -> Result<(), BusFault> {
        self.writes.push((command, bytes.to_vec()));
        dispatch(
            &mut self.iface,
            &mut self.staging,
            command as u8,
            Some(bytes),
            &mut [],
            self.transport_max,
        );
        Ok(())
    }

    fn read(&mut self, command: RecoveryCommand, resp: &mut [u8]) -> Result<usize, BusFault> {
        /// Byte 0 of `INDIRECT_FIFO_STATUS` holds the empty bit.
        const FIFO_STATUS_BITS: usize = 0;
        /// Byte 2 of `INDIRECT_FIFO_CTRL` is the low byte of the image size.
        const FIFO_CTRL_IMAGE_SIZE: usize = 2;

        self.reads += 1;
        let len = dispatch(
            &mut self.iface,
            &mut self.staging,
            command as u8,
            None,
            resp,
            self.transport_max,
        );
        match self.read_fault {
            Some((faulted, fault)) if faulted == command => match fault {
                ReadFault::Bus => Err(BusFault),
                ReadFault::Short => Ok(len - 1),
                ReadFault::FifoNotEmpty => {
                    resp[FIFO_STATUS_BITS] &= !IndirectFifoStatus::EMPTY;
                    Ok(len)
                }
                ReadFault::OtherImageSize => {
                    resp[FIFO_CTRL_IMAGE_SIZE] += 1;
                    Ok(len)
                }
            },
            _ => Ok(len),
        }
    }
}

/// What the test source answers for an image index.
pub enum Held {
    Image(SliceImage),
    /// The source holds the image but cannot serve reads yet.
    Fetching,
    /// The source answers not ready to the first `lookups_left` lookups and
    /// serves the image to every later lookup.
    FetchedLater {
        image: SliceImage,
        lookups_left: Cell<u32>,
    },
}

/// An image source over a list, where the position is the image index.
pub struct ImageList(pub Vec<Held>);

impl ImageSource for ImageList {
    fn image(&self, index: ImageIndex) -> ImageLookup<'_> {
        match self.0.get(usize::from(index.get())) {
            Some(Held::Image(image)) => ImageLookup::Ready(image),
            Some(Held::Fetching) => ImageLookup::NotReady,
            Some(Held::FetchedLater {
                image,
                lookups_left,
            }) => match lookups_left.get().checked_sub(1) {
                Some(left) => {
                    lookups_left.set(left);
                    ImageLookup::NotReady
                }
                None => ImageLookup::Ready(image),
            },
            None => ImageLookup::NotHeld,
        }
    }
}

/// Sizes of the images a device with several images requests, by index.
/// Each differs from the others, so a push of the wrong image shows in the
/// staged length.
pub const IMAGE_SIZES: [usize; 3] = [IMAGE_BYTES, 2 * DEVICE_MAX_TRANSFER, 3 * DWORD];

pub fn image_index(index: usize) -> ImageIndex {
    ImageIndex::new(index as u8).expect("the test index fits in 4 bits")
}

/// The device requests `index` as Caliptra does after an activation.
pub fn request_image(device: &mut Device, index: ImageIndex) {
    device.iface.set_device_state(
        DeviceState::RecoveryModeReady,
        RecoveryReason::FlashlessStreamingBoot,
    );
    device
        .iface
        .set_recovery_status(RecoveryState::AwaitingImage, index);
}

pub fn staged_bytes(device: &Device, len: usize) -> Vec<u8> {
    let mut staged = vec![0u8; len];
    device
        .staging
        .read_at(0, &mut staged)
        .expect("the staging region holds the whole image");
    staged
}
