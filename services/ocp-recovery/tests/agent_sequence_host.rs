// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The Recovery Agent sequence for streaming boot, driven against the device
//! the way the BMC drives it.
//!
//! The eRoT is the Device and the BMC is the Recovery Agent. The agent half
//! of every step here goes through [`dispatch`] with a command byte and a
//! payload, which is all the transport hands the device; the device half
//! goes through the owner methods of [`RecoveryInterface`], which the
//! policy side calls between agent accesses. The agent half reads every
//! register through [`dispatch`], and the test decodes the bytes, as an
//! agent would.
//!
//! The transport is out of scope: the interface answers the same bytes
//! over I3C, USB and SMBus, and `transport_max` is the only place the
//! transport shows. The sequence uses the FIFO push path, the one the
//! streaming-boot capability bits advertise.

use openprot_ocp_recovery_api::command::RecoveryCommand;
use openprot_ocp_recovery_api::regs::{
    capability, DeviceReset, DeviceState, ForcedRecovery, ImageIndex, ImageSelection,
    IndirectFifoStatus, ProtCap, RecoveryReason, RecoveryState, RecoveryStatus, ResetControl,
    ACTIVATE_IDLE, ACTIVATE_RECOVERY_IMAGE, FIFO_IDLE, FIFO_RESET, PROT_CAP_MAGIC,
    PROT_CAP_VERSION_MAJOR, PROT_CAP_VERSION_MINOR,
};
use openprot_ocp_recovery_api::ProtocolError;
use openprot_ocp_recovery_server::{
    dispatch, FifoSink, RamStaging, RecoveryInterface, StagingError,
};
use util_io::ByteSource;

/// Largest payload one access of the transport carries. The interface
/// takes this as an argument, so the test states one value and uses it both
/// as the chunk size the agent pushes and as the cap the device enforces.
const MAX_TRANSFER_BYTES: usize = 64;

/// Size of the image the agent streams. A whole number of chunks, so the
/// last push is a full one and the FIFO indices land on the announced size.
const IMAGE_SIZE_BYTES: usize = 4 * MAX_TRANSFER_BYTES;

/// Capacity of the staging region, which has to hold the whole image.
const STAGING_BYTES: usize = IMAGE_SIZE_BYTES;

/// The FIFO indices count 4-byte units.
const DWORD: usize = 4;

/// Largest read any recovery command answers, from the command table.
const MAX_READ_BYTES: usize = 255;

/// The preformatted `DEVICE_ID` block the board lays out. The contents are
/// opaque to the interface, so the test uses a pattern no zeroed
/// response buffer could pass for.
const DEVICE_ID: &[u8] = &[0xa5; 24];

/// `DEVICE_STATUS` byte offsets, as the agent decodes them.
const STATUS_STATE: usize = 0;
const STATUS_PROTOCOL_ERROR: usize = 1;
const STATUS_REASON: usize = 2;
const STATUS_HEARTBEAT: usize = 4;

/// `PROT_CAP` byte offsets.
const PROT_CAP_VERSION: usize = 8;
const PROT_CAP_CAPABILITIES: usize = 10;

/// `RECOVERY_CTRL` byte offsets.
const CTRL_CMS: usize = 0;
const CTRL_IMAGE_SELECTION: usize = 1;
const CTRL_ACTIVATE: usize = 2;

/// `INDIRECT_FIFO_CTRL` byte offsets.
const FIFO_CTRL_CMS: usize = 0;
const FIFO_CTRL_RESET: usize = 1;
const FIFO_CTRL_IMAGE_SIZE: usize = 2;

/// `DEVICE_RESET` byte offsets.
const RESET_CONTROL: usize = 0;
const RESET_FORCED_RECOVERY: usize = 1;
const RESET_INTERFACE_CONTROL: usize = 2;

/// `INDIRECT_FIFO_STATUS` byte offsets.
const FIFO_STATUS_BITS: usize = 0;
const FIFO_STATUS_REGION: usize = 1;
const FIFO_STATUS_WRITE_INDEX: usize = 4;
const FIFO_STATUS_READ_INDEX: usize = 8;

/// `RECOVERY_STATUS` byte 0 splits into the state and the image index.
const RECOVERY_STATE_MASK: u8 = 0x0f;
const RECOVERY_INDEX_SHIFT: u8 = 4;

/// The component memory space the image goes to. The device advertises one.
const CODE_CMS: u8 = 0;

/// A staging region that also records the size the device announced to it.
///
/// `RamStaging` holds the bytes and answers `is_complete`, but `RamStaging`
/// does not record the size the dispatch passes to `begin_image`. The test
/// checks that the dispatch forwards the image size the agent announces, so
/// `StagingSpy` records that size.
struct StagingSpy {
    region: RamStaging<STAGING_BYTES>,
    announced: Option<u64>,
}

impl StagingSpy {
    fn new() -> Self {
        Self {
            region: RamStaging::new(),
            announced: None,
        }
    }
}

impl FifoSink for StagingSpy {
    fn begin_image(&mut self, size_bytes: u64) -> Result<(), StagingError> {
        self.region.begin_image(size_bytes)?;
        self.announced = Some(size_bytes);
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StagingError> {
        self.region.append(bytes)
    }
}

/// The device under test, with the staging region its FIFO drains into.
struct Device {
    iface: RecoveryInterface,
    staging: StagingSpy,
}

impl Device {
    /// A device that advertises streaming boot over the FIFO region.
    fn new() -> Self {
        let prot_cap = ProtCap::new(
            capability::IDENTIFICATION
                | capability::DEVICE_STATUS
                | capability::PUSH_C_IMAGE
                | capability::FLASHLESS_BOOT
                | capability::FIFO_CMS,
            1,
            0,
            0,
        );
        let fifo_status =
            IndirectFifoStatus::code_region(STAGING_BYTES as u32, MAX_TRANSFER_BYTES as u32)
                .expect("the region and the transfer cap divide by 4");
        Self {
            iface: RecoveryInterface::new(prot_cap, DEVICE_ID, fifo_status),
            staging: StagingSpy::new(),
        }
    }

    /// One agent read, which answers the bytes the transport sends back.
    fn agent_read(&mut self, command: RecoveryCommand) -> Vec<u8> {
        let mut resp = [0u8; MAX_READ_BYTES];
        let len = dispatch(
            &mut self.iface,
            &mut self.staging,
            command as u8,
            None,
            &mut resp,
            MAX_TRANSFER_BYTES,
        );
        resp[..len].to_vec()
    }

    /// One agent write. The transport carries no answer back, so what the
    /// device made of it shows in the next read.
    fn agent_write(&mut self, command: RecoveryCommand, bytes: &[u8]) {
        let mut resp = [0u8; 0];
        let len = dispatch(
            &mut self.iface,
            &mut self.staging,
            command as u8,
            Some(bytes),
            &mut resp,
            MAX_TRANSFER_BYTES,
        );
        assert_eq!(len, 0, "a write answers no bytes");
    }

    /// Reads `DEVICE_STATUS`. The read clears the protocol error, so each
    /// call is one agent poll, and the test sees the error only in the bytes
    /// that call returns.
    fn poll_device_status(&mut self) -> Vec<u8> {
        self.agent_read(RecoveryCommand::DeviceStatus)
    }

    /// The protocol error of one `DEVICE_STATUS` poll.
    fn poll_protocol_error(&mut self) -> u8 {
        self.poll_device_status()[STATUS_PROTOCOL_ERROR]
    }

    fn poll_fifo_status(&mut self) -> Vec<u8> {
        self.agent_read(RecoveryCommand::IndirectFifoStatus)
    }
}

fn reason_code(status: &[u8]) -> u16 {
    u16::from_le_bytes([status[STATUS_REASON], status[STATUS_REASON + 1]])
}

fn heartbeat(status: &[u8]) -> u16 {
    u16::from_le_bytes([status[STATUS_HEARTBEAT], status[STATUS_HEARTBEAT + 1]])
}

fn advertised_capabilities(prot_cap: &[u8]) -> u16 {
    u16::from_le_bytes([
        prot_cap[PROT_CAP_CAPABILITIES],
        prot_cap[PROT_CAP_CAPABILITIES + 1],
    ])
}

/// Bits 0 to 3 of `RECOVERY_STATUS` byte 0.
fn recovery_state(status: &[u8]) -> Option<RecoveryState> {
    RecoveryState::from_bits(status[0] & RECOVERY_STATE_MASK)
}

/// Bits 4 to 7 of `RECOVERY_STATUS` byte 0, the image a status refers to.
fn recovery_image_index(status: &[u8]) -> u8 {
    status[0] >> RECOVERY_INDEX_SHIFT
}

/// The write and read indices of `INDIRECT_FIFO_STATUS`, in 4-byte units.
fn fifo_indices(status: &[u8]) -> (u32, u32) {
    let index = |offset: usize| {
        u32::from_le_bytes(
            status[offset..offset + 4]
                .try_into()
                .expect("4 bytes of an index"),
        )
    };
    (
        index(FIFO_STATUS_WRITE_INDEX),
        index(FIFO_STATUS_READ_INDEX),
    )
}

/// The `RECOVERY_CTRL` write that activates the image the agent pushed.
fn activate_write(selection: ImageSelection) -> [u8; 3] {
    let mut write = [0u8; 3];
    write[CTRL_CMS] = CODE_CMS;
    write[CTRL_IMAGE_SELECTION] = selection as u8;
    write[CTRL_ACTIVATE] = ACTIVATE_RECOVERY_IMAGE;
    write
}

/// The `INDIRECT_FIFO_CTRL` write that programs a push of `size_bytes`.
fn fifo_ctrl_write(size_bytes: usize) -> [u8; 6] {
    let mut ctrl = [0u8; 6];
    ctrl[FIFO_CTRL_CMS] = CODE_CMS;
    ctrl[FIFO_CTRL_RESET] = FIFO_RESET;
    let dwords = (size_bytes / DWORD) as u32;
    ctrl[FIFO_CTRL_IMAGE_SIZE..].copy_from_slice(&dwords.to_le_bytes());
    ctrl
}

/// The image the agent streams, a pattern that catches a chunk landing at
/// the wrong offset.
fn image() -> Vec<u8> {
    (0..IMAGE_SIZE_BYTES).map(|i| (i % 251) as u8).collect()
}

#[test]
fn the_agent_streams_an_image_and_polls_the_device_to_a_terminal_status() {
    let mut device = Device::new();

    // Image bytes before the agent has programmed the FIFO: the staging
    // region has no size to hold them against, so the device refuses the
    // write and the agent reads why on its next poll. The poll clears the
    // error, which the second poll shows.
    device.agent_write(RecoveryCommand::IndirectFifoData, &[0u8; DWORD]);
    assert_eq!(
        device.poll_protocol_error(),
        ProtocolError::UnsupportedParameter as u8
    );
    assert_eq!(device.poll_protocol_error(), ProtocolError::None as u8);
    assert!(device.staging.region.staged_is_empty());

    // The device enters recovery mode with no local image to boot.
    device.iface.set_device_state(
        DeviceState::RecoveryModeReady,
        RecoveryReason::FlashlessStreamingBoot,
    );
    device
        .iface
        .set_recovery_status(RecoveryState::AwaitingImage, ImageIndex::FIRST);

    // The agent reads what the device supports before it drives anything.
    let prot_cap = device.agent_read(RecoveryCommand::ProtCap);
    assert_eq!(prot_cap.len(), ProtCap::SIZE);
    assert_eq!(&prot_cap[..PROT_CAP_MAGIC.len()], &PROT_CAP_MAGIC);
    assert_eq!(prot_cap[PROT_CAP_VERSION], PROT_CAP_VERSION_MAJOR);
    assert_eq!(prot_cap[PROT_CAP_VERSION + 1], PROT_CAP_VERSION_MINOR);
    let streaming_boot = capability::FLASHLESS_BOOT | capability::FIFO_CMS;
    assert_eq!(
        advertised_capabilities(&prot_cap) & streaming_boot,
        streaming_boot
    );
    assert_eq!(device.agent_read(RecoveryCommand::DeviceId), DEVICE_ID);

    // The device reports why it is in recovery: it has no image to boot.
    let status = device.poll_device_status();
    assert_eq!(status[STATUS_STATE], DeviceState::RecoveryModeReady as u8);
    assert_eq!(
        reason_code(&status),
        RecoveryReason::FlashlessStreamingBoot as u16
    );
    assert_eq!(status[STATUS_PROTOCOL_ERROR], ProtocolError::None as u8);

    // The agent watches the heartbeat to tell a live device from a hung one.
    let before = heartbeat(&status);
    device.iface.tick_heartbeat();
    assert_eq!(heartbeat(&device.poll_device_status()), before + 1);

    // The agent programs the FIFO for the image it is about to push.
    device.agent_write(
        RecoveryCommand::IndirectFifoCtrl,
        &fifo_ctrl_write(IMAGE_SIZE_BYTES),
    );
    assert_eq!(device.staging.announced, Some(IMAGE_SIZE_BYTES as u64));
    // The reset ran, so the device zeroed the field: the agent polls this
    // byte to learn the reset is done.
    let fifo_ctrl = device.agent_read(RecoveryCommand::IndirectFifoCtrl);
    assert_eq!(fifo_ctrl[FIFO_CTRL_RESET], FIFO_IDLE);
    assert_eq!(fifo_ctrl[FIFO_CTRL_CMS], CODE_CMS);

    let fifo_status = device.poll_fifo_status();
    assert_eq!(fifo_status.len(), IndirectFifoStatus::SIZE);
    assert_eq!(
        fifo_status[FIFO_STATUS_BITS] & IndirectFifoStatus::EMPTY,
        IndirectFifoStatus::EMPTY
    );
    assert_eq!(
        fifo_status[FIFO_STATUS_REGION],
        IndirectFifoStatus::REGION_CODE
    );

    // The image goes up in transport-sized chunks. The device drains each
    // one into staging before it answers, so the FIFO reads empty between
    // chunks and both indices follow the bytes pushed so far.
    let image = image();
    let mut pushed = 0;
    for chunk in image.chunks(MAX_TRANSFER_BYTES) {
        device.agent_write(RecoveryCommand::IndirectFifoData, chunk);
        pushed += chunk.len();
        let fifo_status = device.poll_fifo_status();
        assert_eq!(fifo_status[FIFO_STATUS_BITS] & IndirectFifoStatus::FULL, 0);
        assert_eq!(
            fifo_status[FIFO_STATUS_BITS] & IndirectFifoStatus::EMPTY,
            IndirectFifoStatus::EMPTY
        );
        let drained = (pushed / DWORD) as u32;
        assert_eq!(fifo_indices(&fifo_status), (drained, drained));
    }
    assert_eq!(pushed, IMAGE_SIZE_BYTES);
    assert_eq!(device.poll_protocol_error(), ProtocolError::None as u8);
    assert!(device.staging.region.is_complete());
    let mut staged = vec![0u8; IMAGE_SIZE_BYTES];
    device
        .staging
        .region
        .read_at(0, &mut staged)
        .expect("the whole image is staged");
    assert_eq!(staged, image);

    // The agent activates the image it pushed.
    device.agent_write(
        RecoveryCommand::RecoveryCtrl,
        &activate_write(ImageSelection::FromMemoryWindow),
    );
    let request = device
        .iface
        .take_activation()
        .expect("the agent asked for an activation");
    assert_eq!(request.cms, CODE_CMS);
    assert_eq!(request.selection, Some(ImageSelection::FromMemoryWindow));
    assert!(
        device.iface.take_activation().is_none(),
        "the device acts on one activation once"
    );
    // The device zeroes the whole field, so the agent reads the idle value
    // and not the 0xe a bitwise clear of 0x1 would leave behind.
    let ctrl = device.agent_read(RecoveryCommand::RecoveryCtrl);
    assert_eq!(ctrl[CTRL_ACTIVATE], ACTIVATE_IDLE);
    assert_eq!(ctrl[CTRL_CMS], CODE_CMS);
    assert_eq!(
        ctrl[CTRL_IMAGE_SELECTION],
        ImageSelection::FromMemoryWindow as u8
    );

    // The verify and trial-boot path runs, and the agent follows it.
    device
        .iface
        .set_recovery_status(RecoveryState::BootingImage, ImageIndex::FIRST);
    device.iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    let status = device.agent_read(RecoveryCommand::RecoveryStatus);
    assert_eq!(status.len(), RecoveryStatus::SIZE);
    assert_eq!(recovery_state(&status), Some(RecoveryState::BootingImage));

    // The image booted. Both registers hold a terminal value the agent can
    // stop on. The device writes reason code 0 together with
    // `RunningRecoveryImage`, a state that carries no reason, so the agent
    // reads no stale reason.
    device
        .iface
        .set_recovery_status(RecoveryState::Successful, ImageIndex::FIRST);
    device
        .iface
        .set_device_state(DeviceState::RunningRecoveryImage, RecoveryReason::NoFailure);
    let status = device.agent_read(RecoveryCommand::RecoveryStatus);
    assert_eq!(recovery_state(&status), Some(RecoveryState::Successful));
    assert_eq!(recovery_image_index(&status), ImageIndex::FIRST.get());
    let status = device.poll_device_status();
    assert_eq!(
        status[STATUS_STATE],
        DeviceState::RunningRecoveryImage as u8
    );
    assert_eq!(reason_code(&status), RecoveryReason::NoFailure as u16);
    assert_eq!(status[STATUS_PROTOCOL_ERROR], ProtocolError::None as u8);
}

#[test]
fn a_failed_stage_names_the_image_it_failed_on() {
    let mut device = Device::new();
    let second_stage = ImageIndex::new(1).expect("1 fits the index field");

    device
        .iface
        .set_recovery_status(RecoveryState::AuthenticationError, second_stage);

    // An agent that reads only the state learns the image failed; the index
    // is what tells it which of the stages it pushed.
    let status = device.agent_read(RecoveryCommand::RecoveryStatus);
    assert_eq!(
        recovery_state(&status),
        Some(RecoveryState::AuthenticationError)
    );
    assert_eq!(recovery_image_index(&status), second_stage.get());
}

#[test]
fn an_agent_clears_a_stale_activation_before_it_announces_the_next_image() {
    let mut device = Device::new();

    // The first attempt writes the activation, and the policy side does not
    // call `take_activation`: the agent gave up, or the device reset before
    // `take_activation` ran.
    device.agent_write(
        RecoveryCommand::IndirectFifoCtrl,
        &fifo_ctrl_write(IMAGE_SIZE_BYTES),
    );
    device.agent_write(
        RecoveryCommand::RecoveryCtrl,
        &activate_write(ImageSelection::FromMemoryWindow),
    );

    // An agent that finds a stale activation writes the idle value first.
    // The idle write drops the stale activation, so `take_activation`
    // returns only the activation the agent writes for the next image.
    let mut idle = activate_write(ImageSelection::FromMemoryWindow);
    idle[CTRL_ACTIVATE] = ACTIVATE_IDLE;
    device.agent_write(RecoveryCommand::RecoveryCtrl, &idle);

    assert_eq!(device.poll_protocol_error(), ProtocolError::None as u8);
    let ctrl = device.agent_read(RecoveryCommand::RecoveryCtrl);
    assert_eq!(ctrl[CTRL_ACTIVATE], ACTIVATE_IDLE);
    assert!(device.iface.take_activation().is_none());

    // The next push activates once, for the image the agent announced.
    device.agent_write(
        RecoveryCommand::IndirectFifoCtrl,
        &fifo_ctrl_write(IMAGE_SIZE_BYTES),
    );
    device.agent_write(
        RecoveryCommand::RecoveryCtrl,
        &activate_write(ImageSelection::FromMemoryWindow),
    );
    assert!(device.iface.take_activation().is_some());
    assert!(device.iface.take_activation().is_none());
}

#[test]
fn an_activate_value_of_one_is_refused_and_the_field_keeps_the_idle_value() {
    let mut device = Device::new();

    // 0x1 is what a device that treated byte 2 as a bitwise clear would
    // accept, and it would leave 0xe behind. The write is refused instead,
    // and every byte of the register keeps the value it had.
    let mut write = activate_write(ImageSelection::FromMemoryWindow);
    write[CTRL_ACTIVATE] = 0x1;
    device.agent_write(RecoveryCommand::RecoveryCtrl, &write);

    assert_eq!(
        device.poll_protocol_error(),
        ProtocolError::UnsupportedParameter as u8
    );
    let ctrl = device.agent_read(RecoveryCommand::RecoveryCtrl);
    assert_eq!(ctrl[CTRL_ACTIVATE], ACTIVATE_IDLE);
    assert_eq!(ctrl[CTRL_IMAGE_SELECTION], ImageSelection::None as u8);
    assert!(device.iface.take_activation().is_none());
}

#[test]
fn an_activation_before_the_image_is_complete_still_reaches_the_device() {
    let mut device = Device::new();
    device.agent_write(
        RecoveryCommand::IndirectFifoCtrl,
        &fifo_ctrl_write(IMAGE_SIZE_BYTES),
    );
    device.agent_write(
        RecoveryCommand::IndirectFifoData,
        &image()[..MAX_TRANSFER_BYTES],
    );

    device.agent_write(
        RecoveryCommand::RecoveryCtrl,
        &activate_write(ImageSelection::FromMemoryWindow),
    );

    // The dispatch serves registers and does not decide policy: it hands the
    // activation over with the image short, and the staging region is what
    // says the image is not there yet. The recovery flow has to refuse an
    // incomplete image after the take.
    assert!(device.iface.take_activation().is_some());
    assert!(!device.staging.region.is_complete());
    assert_eq!(device.poll_protocol_error(), ProtocolError::None as u8);
}

#[test]
fn the_agent_arms_forced_recovery_and_the_take_clears_only_the_reset() {
    let mut device = Device::new();

    // The agent commands a reset and selects the forced recovery mode the
    // device boots in after the reset.
    let mut write = [0u8; 3];
    write[RESET_CONTROL] = ResetControl::ResetDevice as u8;
    write[RESET_FORCED_RECOVERY] = ForcedRecovery::RecoveryMode as u8;
    write[RESET_INTERFACE_CONTROL] = DeviceReset::INTERFACE_MASTERING_DISABLED;
    device.agent_write(RecoveryCommand::DeviceReset, &write);

    assert_eq!(
        device.iface.take_reset_request(),
        Some(ResetControl::ResetDevice)
    );
    assert!(
        device.iface.take_reset_request().is_none(),
        "the device performs one reset once"
    );

    // The device cleared the reset it performed. The forced recovery field
    // stays set: a platform reset clears the field, and the device reads the
    // field at the next boot.
    let reset = device.agent_read(RecoveryCommand::DeviceReset);
    assert_eq!(reset[RESET_CONTROL], ResetControl::None as u8);
    assert_eq!(
        reset[RESET_FORCED_RECOVERY],
        ForcedRecovery::RecoveryMode as u8
    );
    assert_eq!(
        device.iface.armed_forced_recovery(),
        Some(ForcedRecovery::RecoveryMode)
    );
}
