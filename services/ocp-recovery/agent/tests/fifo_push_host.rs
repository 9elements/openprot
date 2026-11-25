// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The FIFO push of the Recovery Agent against the device dispatch.
//!
//! [`FifoPush`] plays the Recovery Agent and calls the device through
//! [`dispatch`] alone. The test plays the policy side of the device between
//! steps: it reports recovery pending once the image is complete and writes
//! the end state after the activation, as the recovery flow would. For a
//! device that boots from several images, the test requests the next image
//! as Caliptra does: recovery mode in `DEVICE_STATUS`, then the next image
//! index in `RECOVERY_STATUS`.

use ocp_recovery_test_fixture::*;
use openprot_ocp_recovery_agent::{AgentError, FifoPush, ImageSizeError, Outcome, Phase, Progress};
use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, ImageIndex, ImageSelection, RecoveryReason, RecoveryState,
};
use openprot_ocp_recovery_api::{ProtocolError, RecoveryCommand};
use std::cell::Cell;
use util_io::{ByteReadError, ByteSource};

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

/// How often a test polls a device that reports no end state.
const POLL_ROUNDS: usize = 3;

/// Runs the push of the current image. Leaves the sequence in
/// `PollRecoveryStatus` with the activation taken on the device.
fn push_and_activate(push: &mut FifoPush<'_>, device: &mut Device, scratch: &mut [u8]) {
    let stopped = run_until_it_stops(push, device, scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::AwaitRecoveryPending);
    device.iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    let stopped = run_until_it_stops(push, device, scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::PollRecoveryStatus);
    device
        .iface
        .take_activation()
        .expect("the agent requested the activation");
}

#[test]
fn the_agent_pushes_the_image_of_every_index_the_device_requests() {
    let images = ImageList(
        IMAGE_SIZES
            .iter()
            .map(|&len| Held::Image(SliceImage::pattern(len)))
            .collect(),
    );
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);

    for (index, &len) in IMAGE_SIZES.iter().enumerate() {
        if index != 0 {
            request_image(&mut device, image_index(index));
        }
        push_and_activate(&mut push, &mut device, &mut scratch);
        assert_eq!(push.pushed_bytes(), len as u64);
        assert_eq!(push.activated_images(), index as u32 + 1);
        let Held::Image(image) = &images.0[index] else {
            panic!("the test list holds an image at every index");
        };
        assert_eq!(staged_bytes(&device, len), image.0);
        device
            .iface
            .set_recovery_status(RecoveryState::BootingImage, image_index(index));
    }

    device.iface.set_recovery_status(
        RecoveryState::Successful,
        image_index(IMAGE_SIZES.len() - 1),
    );
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Done(Outcome::Success)));
}

#[test]
fn a_device_that_requests_the_same_index_again_gets_the_same_image_again() {
    let images = ImageList(vec![Held::Image(SliceImage::pattern(IMAGE_BYTES))]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    push_and_activate(&mut push, &mut device, &mut scratch);

    request_image(&mut device, ImageIndex::FIRST);
    push_and_activate(&mut push, &mut device, &mut scratch);

    assert_eq!(push.activated_images(), 2);
    assert_eq!(device.writes_to(RecoveryCommand::IndirectFifoCtrl).len(), 2);
    let Held::Image(image) = &images.0[0] else {
        panic!("the test list holds an image at index 0");
    };
    assert_eq!(staged_bytes(&device, IMAGE_BYTES), image.0);
}

/// Right after the activation the device still reports the request it made
/// before the push. `DEVICE_STATUS` reads recovery pending, not recovery
/// mode, so the agent starts no second push.
#[test]
fn awaiting_image_without_recovery_mode_after_the_activation_starts_no_second_push() {
    let images = ImageList(vec![Held::Image(SliceImage::pattern(IMAGE_BYTES))]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    push_and_activate(&mut push, &mut device, &mut scratch);

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::PollRecoveryStatus);
    assert_eq!(device.writes_to(RecoveryCommand::IndirectFifoCtrl).len(), 1);
}

#[test]
fn an_index_the_source_does_not_hold_returns_image_not_held_and_no_announcement_follows() {
    let images = ImageList(vec![Held::Image(SliceImage::pattern(IMAGE_BYTES))]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    push_and_activate(&mut push, &mut device, &mut scratch);

    let missing = image_index(1);
    request_image(&mut device, missing);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Err(AgentError::ImageNotHeld(missing)));
    assert_eq!(device.writes_to(RecoveryCommand::IndirectFifoCtrl).len(), 1);
}

#[test]
fn a_source_that_is_not_ready_makes_the_sequence_wait_before_the_announcement() {
    let images = ImageList(vec![Held::Fetching]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::ReadRecoveryStatus);
    assert!(device
        .writes_to(RecoveryCommand::IndirectFifoCtrl)
        .is_empty());
}

#[test]
fn an_image_of_a_size_the_fifo_registers_cannot_state_returns_image_size_at_the_request() {
    let odd_len = IMAGE_BYTES + 1;
    let images = ImageList(vec![
        Held::Image(SliceImage::pattern(IMAGE_BYTES)),
        Held::Image(SliceImage::pattern(odd_len)),
    ]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    push_and_activate(&mut push, &mut device, &mut scratch);

    request_image(&mut device, image_index(1));
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Err(AgentError::ImageSize(ImageSizeError(odd_len as u64)))
    );
    assert_eq!(device.writes_to(RecoveryCommand::IndirectFifoCtrl).len(), 1);
}

#[test]
fn fifo_push_new_returns_image_not_held_when_the_device_requests_a_second_image() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    push_and_activate(&mut push, &mut device, &mut scratch);

    request_image(&mut device, image_index(1));
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Err(AgentError::ImageNotHeld(image_index(1))));
}

#[test]
fn healthy_in_device_status_after_an_activation_ends_the_sequence_with_success() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    push_and_activate(&mut push, &mut device, &mut scratch);

    // `RECOVERY_STATUS` holds no end state, so only `DEVICE_STATUS` can end
    // the sequence.
    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);
    device
        .iface
        .set_device_state(DeviceState::Healthy, RecoveryReason::NoFailure);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Done(Outcome::Success)));
}

#[test]
fn boot_failure_in_device_status_after_an_activation_ends_the_sequence_with_the_state() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    push_and_activate(&mut push, &mut device, &mut scratch);

    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);
    device.iface.set_device_state(
        DeviceState::BootFailure,
        RecoveryReason::RecoveryFirmwareAuthFailure,
    );
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Ok(Progress::Done(Outcome::DeviceFailed {
            state: DeviceState::BootFailure
        }))
    );
}

#[test]
fn boot_failure_in_device_status_before_the_push_ends_the_sequence_before_any_write() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::with_capabilities(STREAMING_CAPABILITIES);
    device.iface.set_device_state(
        DeviceState::BootFailure,
        RecoveryReason::RecoveryFirmwareAuthFailure,
    );
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Ok(Progress::Done(Outcome::DeviceFailed {
            state: DeviceState::BootFailure
        }))
    );
    assert!(device.writes.is_empty());
}

/// The device reports no end state in either register after the last image.
/// The sequence waits, and the count of activated images tells the caller
/// that the device took the image.
#[test]
fn a_device_that_reports_no_end_state_leaves_the_sequence_waiting_with_the_activation_count() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    push_and_activate(&mut push, &mut device, &mut scratch);
    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);

    for _ in 0..POLL_ROUNDS {
        let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
        assert_eq!(stopped, Ok(Progress::Waiting));
        assert_eq!(push.phase(), Phase::PollRecoveryStatus);
    }
    assert_eq!(push.activated_images(), 1);
}

#[test]
fn running_recovery_image_in_device_status_after_an_activation_ends_the_sequence_with_success() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    push_and_activate(&mut push, &mut device, &mut scratch);

    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);
    device
        .iface
        .set_device_state(DeviceState::RunningRecoveryImage, RecoveryReason::NoFailure);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Done(Outcome::Success)));
}

#[test]
fn fatal_error_in_device_status_before_recovery_pending_ends_the_sequence_and_no_activation_follows(
) {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::AwaitRecoveryPending);

    device
        .iface
        .set_device_state(DeviceState::FatalError, RecoveryReason::NoFailure);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Ok(Progress::Done(Outcome::DeviceFailed {
            state: DeviceState::FatalError
        }))
    );
    assert_eq!(push.activated_images(), 0);
    assert!(device.iface.take_activation().is_none());
}

#[test]
fn a_failure_state_in_recovery_status_at_the_next_request_ends_the_sequence() {
    let images = ImageList(vec![Held::Image(SliceImage::pattern(IMAGE_BYTES))]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    push_and_activate(&mut push, &mut device, &mut scratch);

    // The device boots the image, reports recovery mode again, and then
    // the failure of the image.
    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);
    device.iface.set_device_state(
        DeviceState::RecoveryModeReady,
        RecoveryReason::FlashlessStreamingBoot,
    );
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::ReadRecoveryStatus);
    device
        .iface
        .set_recovery_status(RecoveryState::Failed, ImageIndex::FIRST);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(
        stopped,
        Ok(Progress::Done(Outcome::Failed {
            state: RecoveryState::Failed,
            image: ImageIndex::FIRST,
        }))
    );
}

#[test]
fn a_source_that_turns_ready_gets_a_second_lookup_and_the_push_follows() {
    let images = ImageList(vec![Held::FetchedLater {
        image: SliceImage::pattern(IMAGE_BYTES),
        lookups_left: Cell::new(1),
    }]);
    let mut device = Device::awaiting_an_image();
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push = FifoPush::with_source(&images, CODE_CMS, ImageSelection::None);
    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);
    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::ReadRecoveryStatus);

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::AwaitRecoveryPending);
    assert_eq!(push.pushed_bytes(), IMAGE_BYTES as u64);
}

/// A device that failed an earlier recovery writes recovery mode to
/// `DEVICE_STATUS` first and awaiting image to `RECOVERY_STATUS` second, so
/// the sequence can read the old failure state and waits.
#[test]
fn a_failure_state_in_recovery_status_before_the_first_activation_makes_the_sequence_wait() {
    let image = SliceImage::pattern(IMAGE_BYTES);
    let mut device = Device::awaiting_an_image();
    device
        .iface
        .set_recovery_status(RecoveryState::Failed, ImageIndex::FIRST);
    let mut scratch = [0u8; DEVICE_MAX_TRANSFER];
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).expect("the image size divides by 4");

    let stopped = run_until_it_stops(&mut push, &mut device, &mut scratch);

    assert_eq!(stopped, Ok(Progress::Waiting));
    assert_eq!(push.phase(), Phase::ReadRecoveryStatus);
}
