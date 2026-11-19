// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! AST10x0 OCP recovery test target
//!
//! Runs the flash server, the OCP recovery device and the recovery flow as
//! userspace processes.

#![no_std]
#![no_main]

use ast10x0_peripherals::scu::pinctrl::PINCTRL_FMC_QUAD;
use ast10x0_peripherals::scu::ScuRegisters;
use console_backend::console_backend_write_all;
use entry as _;
use target_common::{declare_target, TargetInterface};

pub struct Target {}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 OCP Recovery";

    fn main() -> ! {
        // Static pinmux configuration, applied before any process starts so
        // no task ever needs SCU access (avoids cross-task RMW races on the
        // shared pinctrl registers).
        // SAFETY: kernel main() runs once, single-threaded, with exclusive
        // hardware ownership.
        let scu = unsafe { ScuRegisters::new_global_unlocked() };
        scu.apply_pinctrl_group(PINCTRL_FMC_QUAD);

        codegen::start();
        #[expect(clippy::empty_loop)]
        loop {}
    }

    fn shutdown(code: u32) -> ! {
        let sentinel: &[u8] = if code == 0 {
            b"TEST_RESULT:PASS\n"
        } else {
            b"TEST_RESULT:FAIL\n"
        };
        let _ = console_backend_write_all(sentinel);
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
