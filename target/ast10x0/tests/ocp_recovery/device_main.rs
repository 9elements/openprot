// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The serving side of the OCP recovery device, with a Recovery Agent
//! stand-in in place of the transport.
//!
//! The app holds the register file and a slot for 1 push. The stand-in is
//! `FifoPush` from the test-agent crate, which reads and writes the
//! registers through `dispatch`, as a transport server would with the bytes
//! a Recovery Agent sends. The stand-in stops where it waits for recovery
//! pending, because no app in the test image sets that device status. The
//! device process holds no channel to the flash server: the recovery flow
//! app takes the pushed bytes over the `ocp` channel.
//!
//! The serve loop steps the stand-in when the flow sends a take, and every
//! reply empties the slot, so every take finds the next chunk or the end of
//! the image. The stand-in pushes a chunk when INDIRECT_FIFO_STATUS reports
//! the FIFO empty, and `dispatch` sets that bit after every write.

#![no_main]
#![no_std]

use app_ocp_device::handle;
use ocp_recovery_test_proto::{
    pattern, IMAGE_LEN, MAX_TRANSFER, OP_TAKE, REGION_LEN, REPLY_BYTES, REPLY_HEADER_LEN,
    REPLY_IMAGE_COMPLETE,
};
use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, ImageIndex, ImageSelection, IndirectFifoStatus, ProtCap,
    RecoveryReason, RecoveryState,
};
use openprot_ocp_recovery_api::{RecoveryCommand, SMBUS_MAX_PAYLOAD};
use openprot_ocp_recovery_server::{dispatch, FifoSink, RecoveryInterface, StagingError};
use openprot_ocp_recovery_test_agent::{FifoPush, Phase, Progress, RegisterAccess};
use pw_status::{Error, Result};
use userspace::entry;
use userspace::syscall::{self, Signals};
use userspace::time::Instant;
use util_io::{ByteReadError, ByteSource};
use util_ipc::{IpcHandle, IpcHandler};

const DEVICE_ID: &[u8] = &[0u8; 24];

/// The component memory space of the code region.
const CODE_CMS: u8 = 0;

// Each push of the stand-in is 1 access of the transport `dispatch` checks
// the length against.
const _: () = assert!(MAX_TRANSFER <= SMBUS_MAX_PAYLOAD);

/// Holds a push until the flow takes it.
struct PushSlot {
    announced: Option<u64>,
    received: u64,
    buf: [u8; MAX_TRANSFER],
    len: usize,
}

impl PushSlot {
    const fn new() -> Self {
        Self {
            announced: None,
            received: 0,
            buf: [0; MAX_TRANSFER],
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl FifoSink for PushSlot {
    fn begin_image(&mut self, size_bytes: u64) -> core::result::Result<(), StagingError> {
        if size_bytes > REGION_LEN as u64 {
            return Err(StagingError::Overflow);
        }
        self.announced = Some(size_bytes);
        self.received = 0;
        self.len = 0;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> core::result::Result<(), StagingError> {
        let announced = self.announced.ok_or(StagingError::NoImageAnnounced)?;
        let fits_image = self.received + bytes.len() as u64 <= announced;
        // The flow has not taken the previous push yet.
        if !self.is_empty() || !fits_image {
            return Err(StagingError::Overflow);
        }
        let slot = self
            .buf
            .get_mut(..bytes.len())
            .ok_or(StagingError::Overflow)?;
        slot.copy_from_slice(bytes);
        self.len = bytes.len();
        self.received += bytes.len() as u64;
        Ok(())
    }
}

/// The image the stand-in pushes: the pattern the flow app checks in flash.
struct PatternImage;

impl ByteSource for PatternImage {
    fn len(&self) -> u64 {
        IMAGE_LEN as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> core::result::Result<(), ByteReadError> {
        let start = usize::try_from(offset).map_err(|_| ByteReadError::OutOfRange)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(ByteReadError::OutOfRange)?;
        if end > IMAGE_LEN {
            return Err(ByteReadError::OutOfRange);
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = pattern(start.saturating_add(i));
        }
        Ok(())
    }
}

/// The register file and the slot behind `dispatch`, which `Registers` calls
/// as a transport server would: 1 call per access of the Recovery Agent.
struct Registers {
    iface: RecoveryInterface,
    slot: PushSlot,
}

impl RegisterAccess for Registers {
    type Error = core::convert::Infallible;

    fn write(
        &mut self,
        command: RecoveryCommand,
        bytes: &[u8],
    ) -> core::result::Result<(), Self::Error> {
        dispatch(
            &mut self.iface,
            &mut self.slot,
            command as u8,
            Some(bytes),
            &mut [],
            SMBUS_MAX_PAYLOAD,
        );
        Ok(())
    }

    fn read(
        &mut self,
        command: RecoveryCommand,
        resp: &mut [u8],
    ) -> core::result::Result<usize, Self::Error> {
        Ok(dispatch(
            &mut self.iface,
            &mut self.slot,
            command as u8,
            None,
            resp,
            SMBUS_MAX_PAYLOAD,
        ))
    }
}

/// Whether the stand-in has more of the image to push.
enum Push {
    /// The slot holds the next chunk, and the device took the write.
    Chunk,
    /// The device holds the whole image.
    ImageComplete,
}

/// Steps the Recovery Agent stand-in until the slot holds the next chunk or
/// the image is complete.
///
/// The stand-in reads the protocol error in the step after the write, so
/// `push_next_chunk` makes that step before it reports the chunk. The image
/// is complete where the stand-in waits for recovery pending. A wait in any
/// other phase is an error: no app changes the device state between 2 steps
/// of `push_next_chunk`, so the same read would repeat without end.
fn push_next_chunk(
    push: &mut FifoPush<'_>,
    registers: &mut Registers,
    scratch: &mut [u8],
) -> Result<Push> {
    let mut chunk_in_slot = false;
    loop {
        match push.step(registers, scratch).map_err(|_| Error::Internal)? {
            Progress::ChunkWritten(_) => chunk_in_slot = true,
            Progress::Continue if chunk_in_slot => return Ok(Push::Chunk),
            Progress::Continue => {}
            Progress::Waiting if push.phase() == Phase::AwaitRecoveryPending => {
                return Ok(Push::ImageComplete)
            }
            Progress::Waiting | Progress::Done(_) => return Err(Error::Internal),
        }
    }
}

fn serve() -> Result<()> {
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
    let fifo_status = IndirectFifoStatus::code_region(REGION_LEN as u32, MAX_TRANSFER as u32)
        .ok_or(Error::Internal)?;
    let mut iface = RecoveryInterface::new(prot_cap, DEVICE_ID, fifo_status);
    iface.set_device_state(
        DeviceState::RecoveryModeReady,
        RecoveryReason::FlashlessStreamingBoot,
    );
    iface.set_recovery_status(RecoveryState::AwaitingImage, ImageIndex::FIRST);

    let mut registers = Registers {
        iface,
        slot: PushSlot::new(),
    };
    let image = PatternImage;
    let mut push =
        FifoPush::new(&image, CODE_CMS, ImageSelection::None).map_err(|_| Error::Internal)?;
    let mut scratch = [0u8; MAX_TRANSFER];

    let ipc = IpcHandle::new(handle::OCP);
    let mut reply = [0u8; REPLY_HEADER_LEN + MAX_TRANSFER];
    loop {
        syscall::object_wait(handle::OCP, Signals::READABLE, Instant::MAX)?;
        let mut request = [0u8; 1];
        if ipc.read(0, &mut request)? != request.len() || request != [OP_TAKE] {
            return Err(Error::InvalidArgument);
        }

        let next = push_next_chunk(&mut push, &mut registers, &mut scratch)?;
        let slot = &mut registers.slot;

        match next {
            Push::ImageComplete => ipc.respond(&[REPLY_IMAGE_COMPLETE])?,
            Push::Chunk => {
                let pushed = slot.buf.get(..slot.len).ok_or(Error::Internal)?;
                let (kind, bytes) = reply.split_at_mut(REPLY_HEADER_LEN);
                kind.fill(REPLY_BYTES);
                bytes
                    .get_mut(..pushed.len())
                    .ok_or(Error::Internal)?
                    .copy_from_slice(pushed);
                let len = REPLY_HEADER_LEN + pushed.len();
                ipc.respond(reply.get(..len).ok_or(Error::Internal)?)?;
                slot.len = 0;
            }
        }
    }
}

#[entry]
fn entry() -> Result<()> {
    // `serve` returns only on an error: the flow app ends the test.
    let result = serve();
    pw_log::error!("ocp device stopped");
    let _ = syscall::debug_shutdown(result);
    result
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = syscall::debug_shutdown(Err(Error::Internal));
    #[expect(clippy::empty_loop)]
    loop {}
}
