# What the QEMU integration tests do not cover

Written 2026-10-05. The scenarios and how to run them are in README.md;
this is the list of what a green run still does not say.

## The two halves have met, in one scenario

`full_update` closes this: five apps in one image, an update over PLDM
followed by the device being reset into it and reporting ready a second
time. Checked against its negative, a device that stops reporting after
the post-activation reset.

It carries two negatives of its own: a device that never comes up, and a
device that takes the update and never comes back. `mock_bmc` and
`pldm_update` keep theirs.

## The lines are IPC channels, not GPIOs

Reset and boot-worked are board traces between two chips. QEMU models one
chip and the mock BMC is a process inside it, so there is no second
device for a pin to reach. The adapters in `board/src/bmc.rs` are swapped
out at the trait seam rather than exercised. Pin wiring is what the
hardware tests are for.

## It is not a black box

The orchestrator runs as an app inside the image and the scenarios assert
on its state from in there. The only observations from outside the guest
are the UART sentinel and the CS1 backing file, and only the happy path
checks the latter.

## Against the demo script in PR 518

`docs/ocp-demo.md`, proposed in OpenPRoT/openprot#518, is the sequence the
demo is measured by. It is docs only and still open, so this is a
comparison and not yet a list of defects.

Covered by `full_update`: the RoT boots first and holds the device in
reset, releases it, supervises the walk, discovers the terminus over Type 0,
takes a Type 5 update through inventory, request, pass-component,
update-component, the data loop, transfer, verify and apply, authenticates
the new blob, resets the device, and sees it boot what it was given.

Not covered, in the order they would have to be decided:

- Activation. The 518 Type 5 diagram ends at ApplyComplete, with
  `ComponentActivationMethods.Automatic` in the parameter table and no
  ActivateFirmware anywhere. The device advertises self-contained
  activation and the agent does send ActivateFirmware. That is a different
  flow, not a different flag: under 518 the RoT writing the image and
  resetting the device is the activation, the firmware device's
  `activate()` never runs, and the `PerformActivate` decision in the gate
  has nothing producing it.
- The pending-reset handshake: a pending reset signal to the device, the
  device preparing for shutdown, and an acknowledgement. This exists
  nowhere, not in DSP0267, not in pldm-lib, not in the orchestrator.
- Authenticating the running image before the first release. The verifier
  reads nothing and the device table carries no layout, so the first boot
  is unverified. Doing it for real means two processes mapping the FMC,
  with the firmware device writing CS1 while the RoT reads it.
- Updating the backup partition after the commit. #512 merged, but the
  component here binds `SvnFloorBinding::SelfManaged`, and since 993219b8
  a self-managed component's slots are left to the device. The re-sync
  cannot trigger until a scenario variant binds an eRoT floor.
- Boot progress over MCTP, and the shape of the other side. In 518 the
  managed device is one device that is the update agent, the reset target
  and the progress reporter at once. Here those are two apps, and progress
  arrives on an IPC channel.

## Arcs of the update state machine with no scenario

- Cancel mid-transfer. The agent's `CancelUpdate` and the `AckCancel` that
  answers it. The device's IPC handler refuses `ack_cancel` today.
- `UpdateSecurityRevision` and the `SvnCommitPending` status, so
  `commit_self_svn_floor` has no path that reaches it.
- Recovery preempting an update: `Updating` to `Recovering`, with the
  staged image discarded.
- The spare slot re-sync after a commit, and the floor advance held until
  the spare has the image.

## What the commit-timeout scenario does not say

`full_update/commit_times_out` arms the commit watchdog from the scenario's
own run loop, through the shipped `BootWatchdogs`. Nothing in the shipped
image arms it: `arm_commit` has no caller outside tests, so the commit-or-lock
bound the state machine promises is not wired anywhere yet. The scenario
proves the state machine and the timer against the kernel clock with a real
activation ahead of them, not that the shipped orchestrator would lock.

`tests/orchestrator/runtime` scenario 6 already covers the timer in
isolation, with no update in flight. What this adds is the window opened by
an activation and a walk running through it.

There is no floor to watch it fail to advance. The component binds
`SvnFloorBinding::SelfManaged`, so the eRoT holds no floor here, and
`Effect::LatchLockdown` returns an error from the driver rather than
latching anything. The claim is the state: walk green, no confirmation,
window expired, machine `Locked`.

## Assertions that are looser than they look

`pldm_update` has no managed device, so its `BootConfirmed` is a
stand-in: it is dispatched once the activation is acknowledged, not once
anything booted. `full_update` is where that claim is actually tested.

The same scenario checks the machine is back in `Ready` after the update.
Since `UpdateVerified` enters `PreSupervision`, the path back to `Ready`
runs through verification, and the stub verifier passes synchronously, so
that check would hold even with no boot walk ever polled.

## Known weaknesses in the test apps

- In `full_update` the RoT owns the sentinel, so a firmware-device failure
  after the activation is acknowledged shows in the log but does not fail
  the run. The orchestrator's own path covers the cases that matter today,
  because every step needs the device's acknowledgement and a missing one
  times out, and the host checks CS1 afterwards. It does not cover the
  device's consent flags.

- The RoT's own verifier reads the staging region, which in this scenario
  is a synthetic pattern rather than the bytes the device actually staged.
  The device checks the real bytes; the RoT checks a stand-in.
- A refusal is recorded rather than enforced. The device stops its own
  flow, but the orchestrator has no way to make the agent's request fail
  with a PLDM completion code.
- `query_status` reports `PhaseFailed` with a fixed phase and result code
  rather than the values the device sent the agent.
- The RoT does not wait on the device's `USER` signal. It works because a
  `QueryStatus` sits in the channel until the device serves it and this
  RoT has nothing else to do, but a real event loop would wait on the
  signal.

## Things that bit us, kept here so they are not rediscovered

- A scenario asserting `State::Ready` passed with the device wedged. The
  pass condition has to be the thing being tested, not a state that is
  reached anyway.
- Three apps were calling `debug_shutdown`, so two sentinels reached the
  console and the runner graded whichever came first.
- The agent calls `handle_component` twice, once to pass the component
  table and once to start the component.
- An app whose thread stack is too small dies before its first log line,
  with no panic and no warning.
- An app that is misaligned or oversized still builds; the only sign is a
  PMSAv7 subregion overlap warning on the console.
- Whichever app owns the sentinel decides the run. `full_update` passed
  while the device never rebooted, because the firmware device was still
  declaring the verdict and its own flow had finished.
