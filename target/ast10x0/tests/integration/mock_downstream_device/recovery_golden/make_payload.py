# Licensed under the Apache-2.0 license
# SPDX-License-Identifier: Apache-2.0

"""Writes an image payload for the golden image scenario.

`good` writes the bytes `expected_byte` in orchestrator_main.rs accepts.
`corrupt` writes the same bytes with the last byte inverted, so a verifier
that stops before the end of the image accepts it and the scenario fails.

The byte rule repeats `expected_byte`. The guest cannot read a host file
and the host cannot run guest code, so each side holds a copy of the rule.
"""

import sys

SIZE = 1024


def main() -> None:
    kind, path = sys.argv[1], sys.argv[2]
    payload = bytearray(offset % 251 for offset in range(SIZE))
    if kind == "corrupt":
        payload[-1] ^= 0xFF
    elif kind != "good":
        sys.exit(f"unknown payload kind {kind}")
    with open(path, "wb") as f:
        f.write(payload)


if __name__ == "__main__":
    main()
