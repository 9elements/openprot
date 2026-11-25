// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! A Recovery Agent: the FIFO push of the images a device requests, as a
//! step function.
//!
//! [`FifoPush::step`] makes at most 1 register access per call and never
//! sleeps, so the same sequence runs in a host test and inside a kernel app.
//! The caller decides how long to wait and when to give up: a step that finds
//! the device or the image source not ready returns [`Progress::Waiting`],
//! and the next step reads the register of [`FifoPush::phase`].
//!
//! The writes and their order follow the BMC model of the Caliptra test
//! benches: `RECOVERY_CTRL` 0, 0, 0 once the device reports recovery mode,
//! `INDIRECT_FIFO_CTRL` with the reset and the image size, a chunk to
//! `INDIRECT_FIFO_DATA` whenever the FIFO is empty, and `RECOVERY_CTRL` with
//! the activation once the device reports recovery pending. The reads of
//! `PROT_CAP`, `DEVICE_STATUS` and `RECOVERY_STATUS` that gate those writes
//! are bus reads in the Caliptra model too.
//!
//! `FifoPush` differs from the Caliptra model in these points:
//!
//! - The FPGA model of Caliptra reads the `INDIRECT_FIFO_CTRL` readback and
//!   the empty bit from the registers of the I3C core directly, past the
//!   bus. `FifoPush` reads `INDIRECT_FIFO_CTRL` and `INDIRECT_FIFO_STATUS`
//!   over the bus, as an agent without direct register access has to.
//! - The Caliptra state machine ends when `DEVICE_STATUS` reports healthy or
//!   running the recovery image. `FifoPush` ends on those states too after
//!   an activation, and on healthy before the push.
//!   `FifoPush` also ends on the end state of `RECOVERY_STATUS`, which can
//!   name a failure, and on a `DEVICE_STATUS` of boot failure or fatal
//!   error.
//! - `FifoPush` requires the push and FIFO capability bits of `PROT_CAP`,
//!   where the Caliptra state machine checks the device status bit alone.
//! - `FifoPush` reads `DEVICE_STATUS` after every write, because a write
//!   carries no answer and the protocol error is the only report of a
//!   refused write.
//!
//! A device that boots from several images reports recovery mode again
//! after an activation and names the next image index in `RECOVERY_STATUS`.
//! [`FifoPush::with_source`] takes an [`ImageSource`] and pushes the image of
//! every index the device requests. [`FifoPush::new`] pushes 1 image.
//!
//! A device may report no end state in either register after the last
//! image. The sequence then keeps returning [`Progress::Waiting`] after the
//! activation, and [`FifoPush::activated_images`] tells the caller how many
//! images the device took.
//!
//! The crate holds no transport framing and no retry policy.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use openprot_ocp_recovery_api::regs::{
    capability, DeviceState, DeviceStatus, ImageIndex, ImageSelection, IndirectFifoCtrl,
    IndirectFifoStatus, ProtCap, RecoveryCtrl, RecoveryState, RecoveryStatus, ACTIVATE_IDLE,
    ACTIVATE_RECOVERY_IMAGE, FIFO_RESET, PROT_CAP_MAGIC,
};
use openprot_ocp_recovery_api::{ProtocolError, RecoveryCommand};
use util_io::{ByteReadError, ByteSource};
use zerocopy::FromBytes;

/// The FIFO registers count 4-byte units.
const DWORD: u64 = 4;

/// Largest answer a recovery command returns, from the command table.
const MAX_READ_BYTES: usize = 255;

/// The capabilities the sequence needs: it polls `DEVICE_STATUS` and pushes
/// through the FIFO.
const REQUIRED_CAPABILITIES: u16 =
    capability::DEVICE_STATUS | capability::PUSH_C_IMAGE | capability::FIFO_CMS;

/// The register accesses a Recovery Agent makes to the device.
///
/// A host test and the QEMU app implement `RegisterAccess` with a direct
/// call to the device dispatch. An implementor on a bus adds the framing of
/// that bus.
pub trait RegisterAccess {
    /// Why an access did not complete. The device refusing a write is not an
    /// access error: the device reports the refusal in `DEVICE_STATUS`.
    type Error;

    /// Writes `bytes` to `command`.
    fn write(&mut self, command: RecoveryCommand, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Reads `command` into `resp` and returns how many bytes the device
    /// answered.
    fn read(&mut self, command: RecoveryCommand, resp: &mut [u8]) -> Result<usize, Self::Error>;
}

/// What an [`ImageSource`] answers for an image index.
pub enum ImageLookup<'a> {
    /// The image. The sequence checks the size and pushes the image.
    Ready(&'a dyn ByteSource),
    /// The source holds the image but cannot serve reads yet, for example
    /// while a fetch runs. The sequence waits and looks the image up again.
    NotReady,
    /// The source holds no image for the index.
    NotHeld,
}

/// The images a Recovery Agent can push, by the image index the device
/// requests in `RECOVERY_STATUS`.
///
/// The source decides where an image comes from (flash, a fetch from another
/// component, a test buffer), so `FifoPush` needs only the lookup.
pub trait ImageSource {
    /// Looks up the image for `index`. `FifoPush` calls `image` on every
    /// step while the device waits for the image, so a source that is still
    /// fetching returns [`ImageLookup::NotReady`] and does not block.
    fn image(&self, index: ImageIndex) -> ImageLookup<'_>;
}

/// The access the next [`FifoPush::step`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Read `PROT_CAP` and check the capabilities the push needs.
    ReadProtCap,
    /// Read `DEVICE_STATUS` until the device reports recovery mode, healthy
    /// or a failure state.
    ReadDeviceStatus,
    /// Write `RECOVERY_CTRL` with no activation, which clears an activation
    /// an earlier sequence left behind.
    ClearRecoveryCtrl,
    /// Read `RECOVERY_STATUS` until the device waits for an image, then look
    /// up the image for the index the device requests. Before the first
    /// activation a failure state makes the step return
    /// [`Progress::Waiting`], because the device can still hold the failure
    /// state of an earlier sequence. After an activation a failure state
    /// ends the sequence with [`Outcome::Failed`].
    ReadRecoveryStatus,
    /// Write `INDIRECT_FIFO_CTRL` with the reset and the image size.
    AnnounceImage,
    /// Read `INDIRECT_FIFO_CTRL` back and compare the image size.
    ReadAnnouncement,
    /// Read `INDIRECT_FIFO_STATUS` until the FIFO is empty.
    ReadFifoStatus,
    /// Write the next chunk to `INDIRECT_FIFO_DATA`.
    WriteChunk,
    /// Read `DEVICE_STATUS` for the protocol error of the last write.
    ConfirmWrite,
    /// Read `DEVICE_STATUS` until the device reports recovery pending or a
    /// failure state. The device has taken the last chunk at this point.
    AwaitRecoveryPending,
    /// Write `RECOVERY_CTRL` with the activation.
    Activate,
    /// Read `RECOVERY_STATUS` for an end state. Without an end state the
    /// next step reads `DEVICE_STATUS`.
    PollRecoveryStatus,
    /// Read `DEVICE_STATUS` for an end state or for recovery mode, which
    /// means the device requests another image. Without either the next
    /// step reads `RECOVERY_STATUS` again.
    PollDeviceStatus,
    /// The device refused a write. Every further step returns the same
    /// [`AgentError::Protocol`] and makes no access.
    WriteRefused {
        /// The command the sequence wrote.
        command: RecoveryCommand,
        /// `DEVICE_STATUS` byte 1 as the device answered it.
        code: u8,
    },
    /// The sequence ended. Every further step returns the same outcome and
    /// makes no access.
    Done(Outcome),
}

/// How the sequence ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The device reported `Healthy` before the push, so no recovery ran.
    DeviceHealthy,
    /// The device reported the recovery as successful in `RECOVERY_STATUS`,
    /// or reported `Healthy` or `RunningRecoveryImage` in `DEVICE_STATUS`
    /// after an activation.
    Success,
    /// The device reported a failure state for an image.
    Failed {
        /// The failure state of `RECOVERY_STATUS`.
        state: RecoveryState,
        /// The image the failure state refers to.
        image: ImageIndex,
    },
    /// The device reported `BootFailure` or `FatalError` in `DEVICE_STATUS`.
    DeviceFailed {
        /// The failure state of `DEVICE_STATUS`.
        state: DeviceState,
    },
}

/// What a step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The access went through and the sequence moved on.
    Continue,
    /// The step wrote this many image bytes to the FIFO.
    ChunkWritten(usize),
    /// The device is not in the state [`FifoPush::phase`] waits for, or the
    /// image source is not ready. The caller decides how long to wait
    /// before the next step.
    Waiting,
    /// The sequence ended.
    Done(Outcome),
}

/// Why a step failed.
///
/// After [`AgentError::Protocol`] the sequence is over: the device did not
/// take a write the rest of the sequence builds on, and a refused chunk is
/// missing from the image. After every other error the sequence stays in
/// the phase that failed, and the next step makes the same access again. A
/// chunk write that returned [`AgentError::Access`] may have arrived, in
/// which case the repeated write stores the chunk twice, so a caller on a
/// real bus ends the push after a failed chunk write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentError<E> {
    /// The register access did not complete.
    Access(E),
    /// The device latched a protocol error for a write.
    Protocol {
        /// The command the sequence wrote.
        command: RecoveryCommand,
        /// `DEVICE_STATUS` byte 1 as the device answered it.
        code: u8,
    },
    /// `PROT_CAP` lacks these capability bits.
    MissingCapability(u16),
    /// The answer to this command is too short, or holds a value the spec
    /// reserves.
    Malformed(RecoveryCommand),
    /// `INDIRECT_FIFO_CTRL` reads back another image size than the sequence
    /// wrote.
    AnnouncementMismatch {
        /// The image size the sequence wrote, in bytes.
        announced: u64,
        /// The image size the device answered, in bytes.
        read_back: u64,
    },
    /// The image source failed a read.
    ImageRead(ByteReadError),
    /// The device requests an index the image source does not hold, or a
    /// second image from a `FifoPush` that [`FifoPush::new`] built with 1
    /// image.
    ImageNotHeld(ImageIndex),
    /// The image for the index the device requests has a size the FIFO
    /// registers cannot state.
    ImageSize(ImageSizeError),
    /// The device and the scratch buffer leave no room for 4 bytes of a
    /// chunk.
    NoChunkSpace,
}

impl<E> From<ByteReadError> for AgentError<E> {
    fn from(e: ByteReadError) -> Self {
        Self::ImageRead(e)
    }
}

impl<E> From<ImageSizeError> for AgentError<E> {
    fn from(e: ImageSizeError) -> Self {
        Self::ImageSize(e)
    }
}

/// An image size in bytes that the FIFO registers cannot state: 0, a size
/// that does not divide by 4, or a size past the 32-bit count of 4-byte
/// units in `INDIRECT_FIFO_CTRL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSizeError(pub u64);

/// The image size as the count of 4-byte units `INDIRECT_FIFO_CTRL` takes.
fn image_dwords(image: &dyn ByteSource) -> Result<u32, ImageSizeError> {
    let len = image.len();
    if len == 0 || len % DWORD != 0 {
        return Err(ImageSizeError(len));
    }
    u32::try_from(len / DWORD).map_err(|_| ImageSizeError(len))
}

/// Stands in for the image until the device requests an index.
struct NoImage;

impl ByteSource for NoImage {
    fn len(&self) -> u64 {
        0
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<(), ByteReadError> {
        Err(ByteReadError::OutOfRange)
    }
}

/// Where the sequence takes the image for a requested index from.
/// [`FifoPush::new`] takes 1 image and has no lookup, so a second request
/// finds no image to push.
#[derive(Clone, Copy)]
enum Images<'a> {
    One,
    Source(&'a dyn ImageSource),
}

/// The push of the images a device requests through the FIFO, from the
/// capability read to the end state the device reports.
pub struct FifoPush<'a> {
    images: Images<'a>,
    image: &'a dyn ByteSource,
    image_dwords: u32,
    activated: u32,
    cms: u8,
    selection: ImageSelection,
    phase: Phase,
    pushed: u64,
    max_transfer: u64,
    written: RecoveryCommand,
    after_write: Phase,
}

impl<'a> FifoPush<'a> {
    /// Prepares the push of `image` into the component memory space `cms`.
    ///
    /// `selection` is byte 1 of the `RECOVERY_CTRL` write that activates the
    /// image. The Caliptra BMC model writes [`ImageSelection::None`].
    pub fn new(
        image: &'a dyn ByteSource,
        cms: u8,
        selection: ImageSelection,
    ) -> Result<Self, ImageSizeError> {
        let image_dwords = image_dwords(image)?;
        Ok(Self {
            images: Images::One,
            image,
            image_dwords,
            activated: 0,
            cms,
            selection,
            phase: Phase::ReadProtCap,
            pushed: 0,
            max_transfer: 0,
            written: RecoveryCommand::RecoveryCtrl,
            after_write: Phase::ReadRecoveryStatus,
        })
    }

    /// Prepares the push of every image the device requests from `source`
    /// into the component memory space `cms`.
    ///
    /// `selection` is byte 1 of each `RECOVERY_CTRL` write that activates an
    /// image. `with_source` returns no `Result`, because the sequence checks
    /// the size of an image when the device requests the index, and returns
    /// [`AgentError::ImageSize`] from that step.
    pub fn with_source(source: &'a dyn ImageSource, cms: u8, selection: ImageSelection) -> Self {
        Self {
            images: Images::Source(source),
            image: &NoImage,
            image_dwords: 0,
            activated: 0,
            cms,
            selection,
            phase: Phase::ReadProtCap,
            pushed: 0,
            max_transfer: 0,
            written: RecoveryCommand::RecoveryCtrl,
            after_write: Phase::ReadRecoveryStatus,
        }
    }

    /// How many images the device activated: the activation writes the
    /// device took without a protocol error.
    pub fn activated_images(&self) -> u32 {
        self.activated
    }

    /// The access the next step makes.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// How many bytes of the current image the sequence has written to the
    /// FIFO, counting a chunk the device refused.
    pub fn pushed_bytes(&self) -> u64 {
        self.pushed
    }

    /// Makes the access of the current phase.
    ///
    /// `scratch` holds a chunk between the image read and the FIFO write. A
    /// chunk is as long as `scratch`, the largest transfer the device
    /// reports and the rest of the image allow.
    pub fn step<R: RegisterAccess>(
        &mut self,
        regs: &mut R,
        scratch: &mut [u8],
    ) -> Result<Progress, AgentError<R::Error>> {
        let mut resp = [0u8; MAX_READ_BYTES];
        match self.phase {
            Phase::ReadProtCap => {
                let command = RecoveryCommand::ProtCap;
                let answer = read(regs, command, &mut resp)?;
                let prot_cap =
                    ProtCap::read_from_bytes(answer).map_err(|_| AgentError::Malformed(command))?;
                if !answer.starts_with(&PROT_CAP_MAGIC) {
                    return Err(AgentError::Malformed(command));
                }
                let missing = REQUIRED_CAPABILITIES & !prot_cap.capabilities();
                if missing != 0 {
                    return Err(AgentError::MissingCapability(missing));
                }
                self.advance(Phase::ReadDeviceStatus)
            }
            Phase::ReadDeviceStatus => match device_state(regs, &mut resp)? {
                DeviceState::Healthy => self.finish(Outcome::DeviceHealthy),
                DeviceState::RecoveryModeReady => self.advance(Phase::ClearRecoveryCtrl),
                state @ (DeviceState::BootFailure | DeviceState::FatalError) => {
                    self.finish(Outcome::DeviceFailed { state })
                }
                _ => Ok(Progress::Waiting),
            },
            Phase::ClearRecoveryCtrl => {
                let ctrl: [u8; RecoveryCtrl::SIZE] =
                    [self.cms, ImageSelection::None as u8, ACTIVATE_IDLE];
                self.write(
                    regs,
                    RecoveryCommand::RecoveryCtrl,
                    &ctrl,
                    Phase::ReadRecoveryStatus,
                )
            }
            Phase::ReadRecoveryStatus => match recovery_status(regs, &mut resp)? {
                (RecoveryState::AwaitingImage, index) => self.select_image(index),
                // Before the first activation a failure state can be left
                // over from an earlier sequence, because the device writes
                // recovery mode to `DEVICE_STATUS` before it writes awaiting
                // image to `RECOVERY_STATUS`. The sequence waits for the
                // device to write awaiting image.
                (
                    state @ (RecoveryState::Failed
                    | RecoveryState::AuthenticationError
                    | RecoveryState::EnterRecoveryError
                    | RecoveryState::InvalidComponentAddressSpace),
                    image,
                ) if self.activated != 0 => self.finish(Outcome::Failed { state, image }),
                _ => Ok(Progress::Waiting),
            },
            Phase::AnnounceImage => {
                let [d0, d1, d2, d3] = self.image_dwords.to_le_bytes();
                let ctrl: [u8; IndirectFifoCtrl::SIZE] = [self.cms, FIFO_RESET, d0, d1, d2, d3];
                self.write(
                    regs,
                    RecoveryCommand::IndirectFifoCtrl,
                    &ctrl,
                    Phase::ReadAnnouncement,
                )
            }
            Phase::ReadAnnouncement => {
                let command = RecoveryCommand::IndirectFifoCtrl;
                let answer = read(regs, command, &mut resp)?;
                let ctrl = IndirectFifoCtrl::read_from_bytes(answer)
                    .map_err(|_| AgentError::Malformed(command))?;
                let announced = self.image.len();
                let read_back = ctrl.image_size_bytes();
                if read_back != announced {
                    return Err(AgentError::AnnouncementMismatch {
                        announced,
                        read_back,
                    });
                }
                self.advance(Phase::ReadFifoStatus)
            }
            Phase::ReadFifoStatus => {
                let command = RecoveryCommand::IndirectFifoStatus;
                let answer = read(regs, command, &mut resp)?;
                let status = IndirectFifoStatus::read_from_bytes(answer)
                    .map_err(|_| AgentError::Malformed(command))?;
                if status.status_bits() & IndirectFifoStatus::EMPTY == 0 {
                    return Ok(Progress::Waiting);
                }
                self.max_transfer = status.max_transfer_bytes();
                self.advance(Phase::WriteChunk)
            }
            Phase::WriteChunk => {
                let chunk = self
                    .chunk_len(scratch.len())
                    .and_then(|len| scratch.get_mut(..len))
                    .ok_or(AgentError::NoChunkSpace)?;
                let len = chunk.len();
                self.image.read_at(self.pushed, chunk)?;
                let command = RecoveryCommand::IndirectFifoData;
                regs.write(command, chunk).map_err(AgentError::Access)?;
                self.pushed = self.pushed.saturating_add(len as u64);
                self.written = command;
                self.after_write = if self.pushed >= self.image.len() {
                    Phase::AwaitRecoveryPending
                } else {
                    Phase::ReadFifoStatus
                };
                self.phase = Phase::ConfirmWrite;
                Ok(Progress::ChunkWritten(len))
            }
            Phase::ConfirmWrite => {
                let command = RecoveryCommand::DeviceStatus;
                let answer = read(regs, command, &mut resp)?;
                let (mut status, _vendor_tail) = DeviceStatus::read_from_prefix(answer)
                    .map_err(|_| AgentError::Malformed(command))?;
                let code = status.take_protocol_error();
                if code != ProtocolError::None as u8 {
                    // The read cleared the error on the device, so the phase
                    // keeps it for every later step.
                    let command = self.written;
                    self.phase = Phase::WriteRefused { command, code };
                    return Err(AgentError::Protocol { command, code });
                }
                if self.after_write == Phase::PollRecoveryStatus {
                    self.activated = self.activated.saturating_add(1);
                }
                self.advance(self.after_write)
            }
            Phase::AwaitRecoveryPending => match device_state(regs, &mut resp)? {
                DeviceState::RecoveryPending => self.advance(Phase::Activate),
                state @ (DeviceState::BootFailure | DeviceState::FatalError) => {
                    self.finish(Outcome::DeviceFailed { state })
                }
                _ => Ok(Progress::Waiting),
            },
            Phase::Activate => {
                let ctrl: [u8; RecoveryCtrl::SIZE] =
                    [self.cms, self.selection as u8, ACTIVATE_RECOVERY_IMAGE];
                self.write(
                    regs,
                    RecoveryCommand::RecoveryCtrl,
                    &ctrl,
                    Phase::PollRecoveryStatus,
                )
            }
            Phase::PollRecoveryStatus => match recovery_status(regs, &mut resp)? {
                (RecoveryState::Successful, _) => self.finish(Outcome::Success),
                (
                    state @ (RecoveryState::Failed
                    | RecoveryState::AuthenticationError
                    | RecoveryState::EnterRecoveryError
                    | RecoveryState::InvalidComponentAddressSpace),
                    image,
                ) => self.finish(Outcome::Failed { state, image }),
                _ => self.advance(Phase::PollDeviceStatus),
            },
            Phase::PollDeviceStatus => match device_state(regs, &mut resp)? {
                DeviceState::Healthy | DeviceState::RunningRecoveryImage => {
                    self.finish(Outcome::Success)
                }
                state @ (DeviceState::BootFailure | DeviceState::FatalError) => {
                    self.finish(Outcome::DeviceFailed { state })
                }
                // The device requests another image.
                DeviceState::RecoveryModeReady => self.advance(Phase::ReadRecoveryStatus),
                _ => {
                    self.phase = Phase::PollRecoveryStatus;
                    Ok(Progress::Waiting)
                }
            },
            Phase::WriteRefused { command, code } => Err(AgentError::Protocol { command, code }),
            Phase::Done(outcome) => Ok(Progress::Done(outcome)),
        }
    }

    /// Takes the image for `index` and moves on to the announcement, with
    /// the count of pushed bytes back at 0. While the source is not ready
    /// the phase stays, so the next step reads `RECOVERY_STATUS` again.
    fn select_image<E>(&mut self, index: ImageIndex) -> Result<Progress, AgentError<E>> {
        match self.images {
            // `FifoPush::new` holds 1 image, which the first activation used.
            Images::One if self.activated != 0 => return Err(AgentError::ImageNotHeld(index)),
            Images::One => {}
            Images::Source(source) => match source.image(index) {
                ImageLookup::Ready(image) => {
                    self.image_dwords = image_dwords(image)?;
                    self.image = image;
                }
                ImageLookup::NotReady => return Ok(Progress::Waiting),
                ImageLookup::NotHeld => return Err(AgentError::ImageNotHeld(index)),
            },
        }
        self.pushed = 0;
        self.advance(Phase::AnnounceImage)
    }

    fn advance<E>(&mut self, next: Phase) -> Result<Progress, AgentError<E>> {
        self.phase = next;
        Ok(Progress::Continue)
    }

    fn finish<E>(&mut self, outcome: Outcome) -> Result<Progress, AgentError<E>> {
        self.phase = Phase::Done(outcome);
        Ok(Progress::Done(outcome))
    }

    /// Writes a register and schedules the `DEVICE_STATUS` read that reports
    /// whether the device took the write.
    fn write<R: RegisterAccess>(
        &mut self,
        regs: &mut R,
        command: RecoveryCommand,
        bytes: &[u8],
        next: Phase,
    ) -> Result<Progress, AgentError<R::Error>> {
        regs.write(command, bytes).map_err(AgentError::Access)?;
        self.written = command;
        self.after_write = next;
        self.advance(Phase::ConfirmWrite)
    }

    /// Length of the next chunk, a whole number of 4-byte units. The FIFO
    /// indices count those units and the device rounds a shorter write up,
    /// so a chunk that ends inside a unit would shift every later byte.
    /// `new` refuses an image size that does not divide by 4, so the last
    /// chunk is whole as well. `None` means the device limit or `scratch_len`
    /// leaves no room for a 4-byte unit.
    fn chunk_len(&self, scratch_len: usize) -> Option<usize> {
        let remaining = self.image.len().saturating_sub(self.pushed);
        let room = self.max_transfer.min(scratch_len as u64);
        let len = if remaining <= room {
            remaining
        } else {
            room - room % DWORD
        };
        // `len` is at most `scratch_len`, so the conversion keeps the value.
        usize::try_from(len).ok().filter(|&len| len != 0)
    }
}

/// Reads `command` and returns the bytes the device answered.
fn read<'b, R: RegisterAccess>(
    regs: &mut R,
    command: RecoveryCommand,
    resp: &'b mut [u8],
) -> Result<&'b [u8], AgentError<R::Error>> {
    let len = regs.read(command, resp).map_err(AgentError::Access)?;
    resp.get(..len).ok_or(AgentError::Malformed(command))
}

/// Reads `DEVICE_STATUS` and decodes the device status byte.
fn device_state<R: RegisterAccess>(
    regs: &mut R,
    resp: &mut [u8],
) -> Result<DeviceState, AgentError<R::Error>> {
    let command = RecoveryCommand::DeviceStatus;
    let answer = read(regs, command, resp)?;
    let (status, _vendor_tail) =
        DeviceStatus::read_from_prefix(answer).map_err(|_| AgentError::Malformed(command))?;
    status.state().ok_or(AgentError::Malformed(command))
}

/// Reads `RECOVERY_STATUS` and decodes the state and the image index.
fn recovery_status<R: RegisterAccess>(
    regs: &mut R,
    resp: &mut [u8],
) -> Result<(RecoveryState, ImageIndex), AgentError<R::Error>> {
    let command = RecoveryCommand::RecoveryStatus;
    let answer = read(regs, command, resp)?;
    let status =
        RecoveryStatus::read_from_bytes(answer).map_err(|_| AgentError::Malformed(command))?;
    let state = status.state().ok_or(AgentError::Malformed(command))?;
    Ok((state, status.image()))
}
