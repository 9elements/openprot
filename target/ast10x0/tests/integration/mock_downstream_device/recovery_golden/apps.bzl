# Licensed under the Apache-2.0 license
# SPDX-License-Identifier: Apache-2.0

"""The apps in the golden image scenario, shared with its negative case.

The managed device app and the orchestrator app are the sources of the boot
scenario. The orchestrator app gets the flash client on top, because the
verifier in this scenario reads the image bytes from flash. The flash
server app builds server_main.rs of the flash test.
"""

load(
    "//target/ast10x0/tests/integration/mock_downstream_device:apps.bzl",
    "ORCHESTRATOR_DEPS",
    mock_downstream_device_apps = "APPS",
)

HERE = "//target/ast10x0/tests/integration/mock_downstream_device/recovery_golden:"
PARENT = "//target/ast10x0/tests/integration/mock_downstream_device:"

def _without_orchestrator(apps):
    return [app for app in apps if app["name"] != "orchestrator"]

APPS = _without_orchestrator(mock_downstream_device_apps) + [
    {
        "deps": ORCHESTRATOR_DEPS + [
            "//hal/blocking/flash",
            "//services/flash:client",
            "//util/ipc",
        ],
        "name": "orchestrator",
        "src": PARENT + "orchestrator_main.rs",
    },
    # Holds the FMC and serves reads to the orchestrator app. The name fixes
    # the crate name `app_flash_server` the source imports.
    {
        "deps": [
            "//hal/blocking/flash",
            "//services/flash:server",
            "//target/ast10x0/backend/flash:flash_backend_ast10x0",
            "//util/ipc",
            "@pigweed//pw_kernel/userspace",
            "@pigweed//pw_log/rust:pw_log",
            "@pigweed//pw_status/rust:pw_status",
        ],
        "name": "flash_server",
        "regions": True,
        "src": "//target/ast10x0/tests/flash:server_main.rs",
    },
]

# The kernel applies the FMC pinmux before any process starts, so it needs
# the SoC registers.
TARGET_DEPS = ["//target/ast10x0/peripherals"]

FLASH_SIZE = 64 * 1024 * 1024

# The layout `LAYOUT` in orchestrator_main.rs declares, as the id:base:len
# and base:len strings the flash_image rule takes. Offsets into CS1.
SLOT_0 = "0:0x100000:0x1000"
GOLDEN_REGION = "0x200000:0x1000"
GOLDEN_OFFSET = 0x200000

# CS0 and CS1, because with both present the FMC aperture splits and CS1
# lands at 0x88000000, which is the address the flash server maps.
# w25q512jv is the 64 MiB part.
FLASH = {
    "cs0_image": "cs0.img",
    "cs1_image": "cs1.img",
    "flash_size": FLASH_SIZE,
    "fmc_model": "w25q512jv",
}

SYSTEM_CONFIG = HERE + "system_config"
TARGET_SRC = HERE + "target.rs"
GOOD_PAYLOAD = HERE + "good_payload"
CORRUPT_PAYLOAD = HERE + "corrupt_payload"
