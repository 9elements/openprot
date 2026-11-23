// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The FIFO push of the test Recovery Agent against the device dispatch.
//!
//! [`FifoPush`] plays the Recovery Agent and calls the device through
//! [`dispatch`] alone. The test plays the policy side of the device between
//! steps: it reports recovery pending once the image is complete and writes
//! the end state after the activation, as the recovery flow would.

use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, ImageIndex, ImageSelection, IndirectFifoStatus, ProtCap,
    RecoveryReason, RecoveryState,
};
use openprot_ocp_recovery_api::{ProtocolError, RecoveryCommand};
use openprot_ocp_recovery_server::{dispatch, RamStaging, RecoveryInterface};
use openprot_ocp_recovery_test_agent::{
    AgentError, FifoPush, ImageSizeError, Outcome, Phase, Progress, RegisterAccess,
};
use util_io::{ByteReadError, ByteSource};

/// Largest transfer the device reports in `INDIRECT_FIFO_STATUS`.
const DEVICE_MAX_TRANSFER: usize = 64;

/// The FIFO registers count 4-byte units.
const DWORD: usize = 4;

/// Chunks of the largest transfer in the image.
const FULL_CHUNKS: usize = 3;

/// Length of the last chunk, shorter than the largest transfer.
const LAST_CHUNK_BYTES: usize = 20;

/// Size of the image the agent pushes.
const IMAGE_BYTES: usize = FULL_CHUNKS * DEVICE_MAX_TRANSFER + LAST_CHUNK_BYTES;

/// Period of the image pattern. A prime, so no chunk size divides it and a
/// chunk at the wrong offset holds other bytes.
const PATTERN_PERIOD: usize = 251;

/// Capacity of the staging region.
const STAGING_BYTES: usize = 512;

const DEVICE_ID: &[u8] = &[0xa5; 24];

/// The component memory space of the code region.
const CODE_CMS: u8 = 0;

/// Upper bound on the steps of a test, far above what a push needs. A
/// sequence that never waits, ends or returns an error panics in
/// `run_until_it_stops` at this count.
const STEP_LIMIT: usize = 1000;

const STREAMING_CAPABILITIES: u16 = capability::IDENTIFICATION
    | capability::DEVICE_STATUS
    | capability::PUSH_C_IMAGE
    | capability::FLASHLESS_BOOT
    | capability::FIFO_CMS;

/// An image in RAM.
struct SliceImage(Vec<u8>);

impl SliceImage {
    /// A pattern that shows a chunk the device stored at the wrong offset.
    fn pattern(len: usize) -> Self {
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
struct BrokenImage;

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
struct BusFault;

/// What the test changes in the answer to a read, to produce answers
/// `dispatch` never gives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadFault {
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
struct Device {
    iface: RecoveryInterface,
    staging: RamStaging<STAGING_BYTES>,
    /// Largest payload `dispatch` accepts per access.
    transport_max: usize,
    writes: Vec<(RecoveryCommand, Vec<u8>)>,
    reads: usize,
    /// The fault the next reads of a command carry.
    read_fault: Option<(RecoveryCommand, ReadFault)>,
}

impl Device {
    fn with_capabilities(capabilities: u16) -> Self {
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
    fn awaiting_an_image() -> Self {
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

    fn writes_to(&self, command: RecoveryCommand) -> Vec<&[u8]> {
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

/// Steps until the sequence waits, ends or returns an error, and returns
/// that result.
fn run_until_it_stops(
    push: &mut FifoPush<'_>,
    device: &mut Device,
    scratch: &mut [u8],
) -> Result<Progress, AgentError<BusFault>> {
    for _ in 0..STEP_LIMIT {
        match push.step(device, scratch)? {
            Progress::Continue | Progress::ChunkWritten(_) => {}
            stopped => return Ok(stopped),
        }
    }
    panic!("the sequence did not stop within {STEP_LIMIT} steps");
}

#[test]
fn the_push_lands_the_image_and_ends_with_the_status_the_device_reports() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    // The agent pushes every chunk, then waits for recovery pending.
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::AwaitRecoveryPending);
    assert_eq!(push.pushed_bytes(), IMAGE_BYTES as u64);
    assert!(device.staging.is_complete());
    let mut staged = vec![0u8; IMAGE_BYTES];
    device
        .staging
        .read_at(0, &mut staged)
        .expect("the staging region holds the whole image");
    assert_eq!(staged, image.0);

    // The device reports the image complete, and the agent activates.
    device.iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::PollRecoveryStatus);
    let request = device
        .iface
        .take_activation()
        .expect("the agent requested the activation");
    assert_eq!(request.cms, CODE_CMS);
    assert_eq!(request.selection, Some(ImageSelection::None));

    // The device reports the boot, and the sequence ends.
    device
        .iface
        .set_recovery_status(RecoveryState::Successful, ImageIndex::FIRST);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Done(Outcome::Success)));
    assert_eq!(push.phase(), Phase::Done(Outcome::Success));
}

/// The bytes of every write, as the Caliptra BMC model sends them:
/// `RECOVERY_CTRL` 0, 0, 0 before the push, `INDIRECT_FIFO_CTRL` with CMS 0,
/// reset 1 and the size in 4-byte units, and `RECOVERY_CTRL` 0, 0, 0xf to
/// activate.
#[test]
fn the_writes_carry_the_bytes_the_caliptra_bmc_model_sends() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    run_until_it_stops(&mut push, &mut device, &mut scratch).expect("the push runs");
    device.iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    run_until_it_stops(&mut push, &mut device, &mut scratch).expect("the activation runs");

    let size_dwords = ((IMAGE_BYTES / DWORD) as u32).to_le_bytes();
    let announcement = [
        0x00,
        0x01,
        size_dwords[0],
        size_dwords[1],
        size_dwords[2],
        size_dwords[3],
    ];
    assert_eq!(
        device.writes_to(RecoveryCommand::RecoveryCtrl),
        [&[0x00, 0x00, 0x00][..], &[0x00, 0x00, 0x0f][..]]
    );
    assert_eq!(
        device.writes_to(RecoveryCommand::IndirectFifoCtrl),
        [&announcement[..]]
    );

    // The first write of all is the RECOVERY_CTRL clear, and the chunks
    // follow the announcement in image order.
    assert_eq!(device.writes[0].0, RecoveryCommand::RecoveryCtrl);
    assert_eq!(device.writes[1].0, RecoveryCommand::IndirectFifoCtrl);
    let chunks = device.writes_to(RecoveryCommand::IndirectFifoData);
    assert_eq!(chunks.concat(), image.0);
    let lengths: Vec<usize> = chunks.iter().map(|chunk| chunk.len()).collect();
    assert_eq!(
        lengths,
        [
            DEVICE_MAX_TRANSFER,
            DEVICE_MAX_TRANSFER,
            DEVICE_MAX_TRANSFER,
            LAST_CHUNK_BYTES
        ]
    );
}

#[test]
fn a_scratch_buffer_below_the_device_limit_bounds_the_chunk_to_whole_4_byte_units() {
    /// Bytes of the scratch buffer that hold whole 4-byte units.
    const WHOLE_UNITS: usize = 2 * DWORD;
    /// 2 bytes past the whole units, which no chunk uses.
    const SCRATCH_BYTES: usize = WHOLE_UNITS + 2;

    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; SCRATCH_BYTES];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    run_until_it_stops(&mut push, &mut device, &mut scratch).expect("the push runs");

    let chunks = device.writes_to(RecoveryCommand::IndirectFifoData);
    assert_eq!(chunks.concat(), image.0);
    let (last, full) = chunks.split_last().expect("the push wrote chunks");
    assert!(full.iter().all(|chunk| chunk.len() == WHOLE_UNITS));
    assert_eq!(last.len(), IMAGE_BYTES % WHOLE_UNITS);
}

#[test]
fn a_healthy_device_ends_the_sequence_before_any_write() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::with_capabilities(STREAMING_CAPABILITIES);
    device
        .iface
        .set_device_state(DeviceState::Healthy, RecoveryReason::FlashlessStreamingBoot);
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Done(Outcome::DeviceHealthy)));
    assert!(device.writes.is_empty());
}

#[test]
fn prot_cap_without_the_fifo_bit_returns_missing_capability_and_no_write_follows() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::with_capabilities(STREAMING_CAPABILITIES & !capability::FIFO_CMS);
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Err(AgentError::MissingCapability(capability::FIFO_CMS))
    );
    assert_eq!(push.phase(), Phase::ReadProtCap);
    assert!(device.writes.is_empty());
}

/// The transport carries less than the device reports as the largest
/// transfer, so `dispatch` refuses the first chunk and latches the
/// length error the agent reads next.
#[test]
fn a_refused_chunk_returns_the_protocol_error_on_the_next_step_and_on_every_later_step() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device.transport_max = DEVICE_MAX_TRANSFER / 2;
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let refused = Err(AgentError::Protocol {
        command: RecoveryCommand::IndirectFifoData,
        code: ProtocolError::LengthWriteError as u8,
    });
    assert_eq!(
        run_until_it_stops(&mut push, &mut device, &mut scratch),
        refused
    );
    assert!(device.staging.staged_is_empty());

    // The read cleared the error on the device. The sequence keeps the
    // error and makes no further access.
    let accesses = (device.reads, device.writes.len());
    assert_eq!(push.step(&mut device, &mut scratch), refused);
    assert_eq!((device.reads, device.writes.len()), accesses);
    assert_eq!(device.writes_to(RecoveryCommand::IndirectFifoData).len(), 1);
}

#[test]
fn a_failure_state_after_the_activation_ends_the_sequence_and_names_the_image() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    run_until_it_stops(&mut push, &mut device, &mut scratch).expect("the push runs");
    device.iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    run_until_it_stops(&mut push, &mut device, &mut scratch).expect("the activation runs");

    device
        .iface
        .set_recovery_status(RecoveryState::AuthenticationError, ImageIndex::FIRST);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    let failed = Ok(Progress::Done(Outcome::Failed {
        state: RecoveryState::AuthenticationError,
        image: ImageIndex::FIRST,
    }));
    assert_eq!(stopped, failed);

    // A step after the end returns the outcome again and makes no access.
    let accesses = (device.reads, device.writes.len());
    assert_eq!(push.step(&mut device, &mut scratch), failed);
    assert_eq!((device.reads, device.writes.len()), accesses);
}

/// The staging region is smaller than the image, so `dispatch` refuses the
/// `INDIRECT_FIFO_CTRL` write that announces the image.
#[test]
fn a_refused_announcement_returns_the_protocol_error_and_no_chunk_follows() {
    let image = SliceImage::pattern(STAGING_BYTES + DWORD);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert!(matches!(
        stopped,
        Err(AgentError::Protocol {
            command: RecoveryCommand::IndirectFifoCtrl,
            ..
        })
    ));
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoData)
        .is_empty());
}

#[test]
fn the_agent_writes_no_chunk_until_the_fifo_status_reports_empty() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device.read_fault = Some((RecoveryCommand::IndirectFifoStatus, ReadFault::FifoNotEmpty));
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::ReadFifoStatus);
    assert_eq!(push.step(&mut device, &mut scratch), Ok(Progress::Waiting));
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoData)
        .is_empty());

    device.read_fault = None;
    assert_eq!(push.step(&mut device, &mut scratch), Ok(Progress::Continue));
    assert_eq!(
        push.step(&mut device, &mut scratch),
        Ok(Progress::ChunkWritten(DEVICE_MAX_TRANSFER))
    );
}

#[test]
fn a_readback_of_another_image_size_returns_the_mismatch_and_no_chunk_follows() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device.read_fault = Some((RecoveryCommand::IndirectFifoCtrl, ReadFault::OtherImageSize));
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Err(AgentError::AnnouncementMismatch {
            announced: IMAGE_BYTES as u64,
            read_back: (IMAGE_BYTES + DWORD) as u64,
        })
    );
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoData)
        .is_empty());
}

#[test]
fn a_short_answer_returns_malformed_with_the_command_and_the_next_step_reads_again() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device.read_fault = Some((RecoveryCommand::RecoveryStatus, ReadFault::Short));
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(
        stopped,
        Err(AgentError::Malformed(RecoveryCommand::RecoveryStatus))
    );
    assert_eq!(push.phase(), Phase::ReadRecoveryStatus);

    device.read_fault = None;
    assert_eq!(push.step(&mut device, &mut scratch), Ok(Progress::Continue));
    assert_eq!(push.phase(), Phase::AnnounceImage);
}

#[test]
fn a_failed_access_returns_the_bus_error_and_the_next_step_makes_the_access_again() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device.read_fault = Some((RecoveryCommand::ProtCap, ReadFault::Bus));
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    assert_eq!(
        push.step(&mut device, &mut scratch),
        Err(AgentError::Access(BusFault))
    );
    assert_eq!(push.phase(), Phase::ReadProtCap);

    device.read_fault = None;
    assert_eq!(push.step(&mut device, &mut scratch), Ok(Progress::Continue));
    assert_eq!(push.phase(), Phase::ReadDeviceStatus);
}

#[test]
fn an_image_read_error_returns_before_the_chunk_write() {
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::new(&BrokenImage, CODE_CMS, ImageSelection::None)
        .expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Err(AgentError::ImageRead(ByteReadError::Storage)));
    assert_eq!(push.phase(), Phase::WriteChunk);
    assert_eq!(push.pushed_bytes(), 0);
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoData)
        .is_empty());
}

#[test]
fn a_scratch_buffer_below_4_bytes_returns_no_chunk_space() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DWORD - 1];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Err(AgentError::NoChunkSpace));
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoData)
        .is_empty());
}

#[test]
fn an_image_size_the_fifo_registers_cannot_state_is_refused_before_any_access() {
    /// 1 byte past a whole number of 4-byte units.
    const UNEVEN_BYTES: usize = IMAGE_BYTES + 1;

    let uneven = SliceImage::pattern(UNEVEN_BYTES);
    let empty = SliceImage::pattern(0);

    assert_eq!(
        FifoPush::new(&uneven, CODE_CMS, ImageSelection::None).err(),
        Some(ImageSizeError(UNEVEN_BYTES as u64))
    );
    assert_eq!(
        FifoPush::new(&empty, CODE_CMS, ImageSelection::None).err(),
        Some(ImageSizeError(0))
    );
}
