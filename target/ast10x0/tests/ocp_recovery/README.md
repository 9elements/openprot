# OCP recovery device and recovery flow

A QEMU test image with no orchestrator, holding these apps:

- `flash_server_bin`, the flash server of the `flash_service` test.
- `ocp_device_app`, the serving side: the OCP recovery register file, a slot
  that holds 1 push, and a Recovery Agent stand-in that writes the registers
  through `dispatch` in place of a transport.
- `ocp_flow_app`, the recovery flow: takes the pushed bytes from the device
  app over the `ocp` channel and writes them to CS1 through the flash service.

The device process holds no channel to the flash server, and the flow app has
no dependency on the OCP recovery crates.

The stand-in announces an image, and each take from the flow makes the
stand-in push the next chunk. The flow erases a sector when the write position
enters it, programs the chunk, and at the end reads the image back from flash
and compares it with the bytes the stand-in pushed.

```sh
bazelisk test --config=virt_ast10x0 //target/ast10x0/tests/ocp_recovery:ocp_recovery_qemu_test
```

## What the test does not cover

- The stand-in pushes inside the take, so the device app answers no register
  access while the flow is inside a flash call.
- A take finds bytes or the end of the image every time, so no take returns
  empty and `dispatch` refuses no push.
- The Recovery Agent stand-in sends no activation, and the flow verifies no
  image, writes no status and resets nothing.
- INDIRECT_FIFO_STATUS reads empty after every push, also while the slot
  holds bytes.
