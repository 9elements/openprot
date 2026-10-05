// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use console_backend::console_backend_write_all;
use entry as _;
use target_common::{declare_target, TargetInterface};

pub struct Target;

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 hello world payload";

    fn main() -> ! {
        // Distinct from entry.rs's own greeting so the harness can tell the
        // delivered image apart from whatever was booted before it.
        let _ = console_backend_write_all(b"HELLO_FROM_UPDATED_IMAGE\r\n");
        #[expect(clippy::empty_loop)]
        loop {}
    }

    fn shutdown(_code: u32) -> ! {
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
