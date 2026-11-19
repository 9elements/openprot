// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The serving side of the OCP recovery device, with a Recovery Agent
//! stand-in in place of the transport.
//!
//! The app holds the register file and a slot for 1 push. The stand-in
//! writes the registers through `dispatch`, as a transport server would with
//! the bytes a BMC sends. The device process holds no channel to the flash
//! server: the recovery flow app takes the pushed bytes over the `ocp`
//! channel.
//!
//! The stand-in pushes the next chunk when the flow sends a take and the
//! slot is empty, so every take finds bytes or the end of the image. The
//! stand-in checks whether the slot is empty, because `dispatch` sets the
//! empty bit of INDIRECT_FIFO_STATUS after every write.

#![no_main]
#![no_std]

use app_ocp_device::handle;
use ocp_recovery_test_proto::{
    pattern, DWORD, IMAGE_LEN, MAX_TRANSFER, OP_TAKE, REGION_LEN, REPLY_BYTES, REPLY_HEADER_LEN,
    REPLY_IMAGE_COMPLETE,
};
use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, ImageIndex, IndirectFifoCtrl, IndirectFifoStatus, ProtCap,
    RecoveryReason, RecoveryState, FIFO_RESET,
};
use openprot_ocp_recovery_api::{ProtocolError, RecoveryCommand, SMBUS_MAX_PAYLOAD};
use openprot_ocp_recovery_server::{dispatch, FifoSink, RecoveryInterface, StagingError};
use pw_status::{Error, Result};
use userspace::entry;
use userspace::syscall::{self, Signals};
use userspace::time::Instant;
use util_ipc::{IpcHandle, IpcHandler};

const DEVICE_ID: &[u8] = &[0u8; 24];

/// Byte 1 of `DEVICE_STATUS`, the latched protocol error.
const STATUS_PROTOCOL_ERROR: usize = 1;

/// Byte 1 of an `INDIRECT_FIFO_CTRL` write, the reset field.
const FIFO_CTRL_RESET: usize = 1;
/// Bytes 2 to 5 of an `INDIRECT_FIFO_CTRL` write, the image size.
const FIFO_CTRL_IMAGE_SIZE: usize = 2;

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

/// The Recovery Agent stand-in: what a BMC would send over the transport.
struct Agent {
    pushed: usize,
}

impl Agent {
    /// Writes a command and reads `DEVICE_STATUS` for the protocol error.
    fn write(
        iface: &mut RecoveryInterface,
        slot: &mut PushSlot,
        command: RecoveryCommand,
        bytes: &[u8],
    ) -> Result<()> {
        let mut none = [0u8; 0];
        dispatch(
            iface,
            slot,
            command as u8,
            Some(bytes),
            &mut none,
            SMBUS_MAX_PAYLOAD,
        );
        let mut status = [0u8; 16];
        let len = dispatch(
            iface,
            slot,
            RecoveryCommand::DeviceStatus as u8,
            None,
            &mut status,
            SMBUS_MAX_PAYLOAD,
        );
        match status.get(STATUS_PROTOCOL_ERROR) {
            Some(&e) if len > STATUS_PROTOCOL_ERROR && e == ProtocolError::None as u8 => Ok(()),
            _ => Err(Error::Internal),
        }
    }

    /// Announces the image: a FIFO reset with the size in 4-byte units.
    fn announce(iface: &mut RecoveryInterface, slot: &mut PushSlot) -> Result<()> {
        let mut ctrl = [0u8; IndirectFifoCtrl::SIZE];
        let dwords = ((IMAGE_LEN / DWORD) as u32).to_le_bytes();
        ctrl[FIFO_CTRL_RESET] = FIFO_RESET;
        ctrl[FIFO_CTRL_IMAGE_SIZE..].copy_from_slice(&dwords);
        Self::write(iface, slot, RecoveryCommand::IndirectFifoCtrl, &ctrl)
    }

    fn done(&self) -> bool {
        self.pushed >= IMAGE_LEN
    }

    fn push_next(&mut self, iface: &mut RecoveryInterface, slot: &mut PushSlot) -> Result<()> {
        let n = core::cmp::min(MAX_TRANSFER, IMAGE_LEN - self.pushed);
        let mut chunk = [0u8; MAX_TRANSFER];
        let chunk = chunk.get_mut(..n).ok_or(Error::Internal)?;
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = pattern(self.pushed + i);
        }
        Self::write(iface, slot, RecoveryCommand::IndirectFifoData, chunk)?;
        self.pushed += n;
        Ok(())
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

    let mut slot = PushSlot::new();
    let mut agent = Agent { pushed: 0 };
    Agent::announce(&mut iface, &mut slot)?;
    pw_log::info!("ocp device: image announced");

    let ipc = IpcHandle::new(handle::OCP);
    let mut reply = [0u8; REPLY_HEADER_LEN + MAX_TRANSFER];
    loop {
        syscall::object_wait(handle::OCP, Signals::READABLE, Instant::MAX)?;
        let mut request = [0u8; 1];
        if ipc.read(0, &mut request)? != request.len() || request != [OP_TAKE] {
            return Err(Error::InvalidArgument);
        }

        if slot.is_empty() && !agent.done() {
            agent.push_next(&mut iface, &mut slot)?;
        }

        if slot.is_empty() {
            ipc.respond(&[REPLY_IMAGE_COMPLETE])?;
        } else {
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
