// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The recovery flow: takes the pushed bytes from the OCP recovery device app
//! and writes them to flash through the flash service.
//!
//! Every call here blocks, the flash erase included, which is why the flow
//! is a process apart from the device app. The flow erases a sector when the
//! write position enters it, then reads the whole image back and compares it
//! with the bytes the Recovery Agent stand-in pushed.

#![no_main]
#![no_std]

use app_ocp_flow::handle;
use hal_flash::{Flash, FlashAddress};
use ocp_recovery_test_proto::{
    pattern, IMAGE_LEN, MAX_TRANSFER, OP_TAKE, REGION_LEN, REPLY_BYTES, REPLY_HEADER_LEN,
    REPLY_IMAGE_COMPLETE, SECTOR_LEN,
};
use openprot_flash_client_ipc::{ChannelTransport, FlashIpcClient};
use pw_status::{Error, Result};
use userspace::time::Instant;
use userspace::{entry, syscall};
use util_ipc::{IpcHandle, IpcInitiator};

/// Where the image lands: 1 MiB into CS1, the offset the flash_service test
/// writes at.
const REGION_START: usize = 0x0010_0000;

fn addr(offset: usize) -> FlashAddress {
    FlashAddress::new((REGION_START + offset) as u32)
}

fn run() -> core::result::Result<(), &'static str> {
    let transport = ChannelTransport::new(IpcHandle::new(handle::FLASH));
    let mut flash = FlashIpcClient::new(transport).map_err(|_| "flash client")?;
    let (total, sector, _) = flash.geometry().map_err(|_| "geometry")?;
    let sector_len = sector.get();
    // The image is laid out to cross a sector boundary of this size.
    if sector_len != SECTOR_LEN {
        return Err("unsupported sector size");
    }
    if total.get() < REGION_START + REGION_LEN {
        return Err("flash size does not fit the region");
    }

    let device = IpcHandle::new(handle::OCP);
    let mut reply = [0u8; REPLY_HEADER_LEN + MAX_TRANSFER];
    let mut written = 0usize;
    let mut erased_until = 0usize;
    loop {
        let len = device
            .transact(&[OP_TAKE], &mut reply, Instant::MAX)
            .map_err(|_| "take")?;
        let (kind, bytes) = reply
            .get(..len)
            .and_then(|r| r.split_first())
            .ok_or("empty reply")?;
        match *kind {
            REPLY_BYTES => {
                if written + bytes.len() > REGION_LEN {
                    return Err("push past the region");
                }
                while erased_until < written + bytes.len() {
                    flash
                        .erase(addr(erased_until), sector)
                        .map_err(|_| "erase")?;
                    erased_until += sector_len;
                }
                flash.program(addr(written), bytes).map_err(|_| "program")?;
                written += bytes.len();
            }
            REPLY_IMAGE_COMPLETE => break,
            _ => return Err("unknown reply"),
        }
    }
    if written != IMAGE_LEN {
        return Err("image length");
    }

    let mut chunk = [0u8; MAX_TRANSFER];
    let mut checked = 0usize;
    while checked < IMAGE_LEN {
        let n = core::cmp::min(chunk.len(), IMAGE_LEN - checked);
        let buf = chunk.get_mut(..n).ok_or("chunk")?;
        flash.read(addr(checked), buf).map_err(|_| "read back")?;
        if buf
            .iter()
            .enumerate()
            .any(|(i, &b)| b != pattern(checked + i))
        {
            return Err("flash does not hold the pushed image");
        }
        checked += n;
    }
    Ok(())
}

#[entry]
fn entry() -> Result<()> {
    let result = match run() {
        Ok(()) => {
            pw_log::info!("ocp flow PASS");
            Ok(())
        }
        Err(e) => {
            pw_log::error!("ocp flow FAIL: {}", e as &str);
            Err(Error::Unknown)
        }
    };
    let _ = syscall::debug_shutdown(result);
    result
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = syscall::debug_shutdown(Err(Error::Internal));
    #[expect(clippy::empty_loop)]
    loop {}
}
