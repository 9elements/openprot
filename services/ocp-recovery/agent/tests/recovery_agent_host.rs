// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! [`RecoveryAgent`] against the device dispatch.
//!
//! The tests play the task that hosts the agent: they hand in the posts of
//! the orchestrator side, poll, and take the reports. Between polls the
//! tests play the policy side of the device, as the tests of `FifoPush` do.

use ocp_recovery_test_fixture::*;
use openprot_ocp_recovery_agent::{
    AgentError, BootCyclePost, Outcome, Phase, PushLimits, PushReport, PushStatus, RecoveryAgent,
    RegisterAccess, Served,
};
use openprot_ocp_recovery_api::regs::{DeviceState, ImageSelection, RecoveryReason, RecoveryState};
use openprot_ocp_recovery_api::{ProtocolError, RecoveryCommand};
use std::cell::RefCell;

/// The limits most tests use. `max_images` lets every image of
/// [`IMAGE_SIZES`] through.
const LIMITS: PushLimits = PushLimits {
    max_images: IMAGE_SIZES.len() as u32,
    max_restarts: 2,
};

/// The boot cycle number of the first `Released` post in a test.
const FIRST_CYCLE: u32 = 7;

/// The register access the agent owns. The test keeps the `RefCell` to play
/// the device between polls.
struct Bus<'d>(&'d RefCell<Device>);

impl RegisterAccess for Bus<'_> {
    type Error = BusFault;

    fn write(&mut self, command: RecoveryCommand, bytes: &[u8]) -> Result<(), BusFault> {
        self.0.borrow_mut().write(command, bytes)
    }

    fn read(&mut self, command: RecoveryCommand, resp: &mut [u8]) -> Result<usize, BusFault> {
        self.0.borrow_mut().read(command, resp)
    }
}

type Agent<'a> = RecoveryAgent<'a, Bus<'a>, DEVICE_MAX_TRANSFER>;

fn agent<'a>(device: &'a RefCell<Device>, images: &'a ImageList, limits: PushLimits) -> Agent<'a> {
    RecoveryAgent::new(Bus(device), images, CODE_CMS, ImageSelection::None, limits)
}

/// The first `count` images of [`IMAGE_SIZES`], by image index.
fn images(count: usize) -> ImageList {
    ImageList(
        IMAGE_SIZES[..count]
            .iter()
            .map(|&len| Held::Image(SliceImage::pattern(len)))
            .collect(),
    )
}

/// Polls while the accesses go through, and returns the first other answer.
fn poll_until_it_rests(agent: &mut Agent<'_>) -> Served<BusFault> {
    for _ in 0..STEP_LIMIT {
        match agent.poll() {
            Served::Advanced => {}
            rested => return rested,
        }
    }
    panic!("the agent did not rest within {STEP_LIMIT} polls");
}

/// Polls through the push of the current image, with the device reporting
/// recovery pending and taking the activation.
fn push_and_activate(agent: &mut Agent<'_>, device: &RefCell<Device>) {
    assert_eq!(poll_until_it_rests(agent), Served::Waiting);
    assert_eq!(agent.sequence().phase(), Phase::AwaitRecoveryPending);
    device.borrow_mut().iface.set_device_state(
        DeviceState::RecoveryPending,
        RecoveryReason::FlashlessStreamingBoot,
    );
    assert_eq!(poll_until_it_rests(agent), Served::Waiting);
    assert_eq!(agent.sequence().phase(), Phase::PollRecoveryStatus);
    device
        .borrow_mut()
        .iface
        .take_activation()
        .expect("the agent requested the activation");
}

/// Every report the agent has due, oldest first.
fn reports(agent: &mut Agent<'_>) -> Vec<PushReport> {
    std::iter::from_fn(|| agent.take_report()).collect()
}

fn accesses(device: &RefCell<Device>) -> (usize, usize) {
    let device = device.borrow();
    (device.reads, device.writes.len())
}

fn image_bytes(images: &ImageList, index: usize) -> &[u8] {
    let Held::Image(image) = &images.0[index] else {
        panic!("the test list holds an image at index {index}");
    };
    &image.0
}

#[test]
fn the_agent_makes_no_access_before_a_released_post_and_after_a_held_post() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    let mut agent = agent(&device, &images, LIMITS);

    assert_eq!(agent.poll(), Served::Idle);
    assert_eq!(accesses(&device), (0, 0));

    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    assert_eq!(agent.poll(), Served::Advanced);
    let after_one_poll = accesses(&device);
    assert_eq!(after_one_poll, (1, 0));

    agent.post(BootCyclePost::Held);
    assert_eq!(agent.poll(), Served::Idle);
    assert_eq!(accesses(&device), after_one_poll);
    assert_eq!(agent.take_report(), None);
}

#[test]
fn after_a_released_post_the_agent_pushes_every_requested_image_and_is_idle_after_the_end_state() {
    let images = images(IMAGE_SIZES.len());
    let device = RefCell::new(Device::awaiting_an_image());
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });

    for (index, &len) in IMAGE_SIZES.iter().enumerate() {
        if index != 0 {
            request_image(&mut device.borrow_mut(), image_index(index));
        }
        push_and_activate(&mut agent, &device);
        assert_eq!(
            staged_bytes(&device.borrow(), len),
            image_bytes(&images, index)
        );
    }
    device
        .borrow_mut()
        .iface
        .set_device_state(DeviceState::Healthy, RecoveryReason::NoFailure);

    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Ended(Outcome::Success)
    );
    assert_eq!(
        reports(&mut agent).last(),
        Some(&PushReport {
            cycle: FIRST_CYCLE,
            status: PushStatus::Ended(Outcome::Success),
        })
    );

    // The device requests an image again, and no post released a new boot
    // cycle, so the agent makes no access.
    request_image(&mut device.borrow_mut(), image_index(0));
    let at_the_end = accesses(&device);
    assert_eq!(agent.poll(), Served::Idle);
    assert_eq!(accesses(&device), at_the_end);
}

#[test]
fn a_released_post_in_the_middle_of_a_push_starts_at_prot_cap_and_stages_the_image_once() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    for _ in 0..STEP_LIMIT {
        if agent.sequence().pushed_bytes() != 0 {
            break;
        }
        assert_eq!(agent.poll(), Served::Advanced);
    }
    assert_ne!(agent.sequence().pushed_bytes(), 0);

    // The orchestrator resets the device and releases it again.
    *device.borrow_mut() = Device::awaiting_an_image();
    let next_cycle = FIRST_CYCLE + 1;
    agent.post(BootCyclePost::Released { cycle: next_cycle });
    assert_eq!(agent.sequence().phase(), Phase::ReadProtCap);

    push_and_activate(&mut agent, &device);
    assert_eq!(
        staged_bytes(&device.borrow(), IMAGE_SIZES[0]),
        image_bytes(&images, 0)
    );
    assert_eq!(
        device
            .borrow()
            .writes_to(RecoveryCommand::IndirectFifoCtrl)
            .len(),
        1
    );
    assert_eq!(
        reports(&mut agent),
        [PushReport {
            cycle: next_cycle,
            status: PushStatus::NoEndState { activated: 1 },
        }]
    );
}

#[test]
fn a_released_post_after_an_end_state_starts_a_fresh_push() {
    let images = images(1);
    let device = RefCell::new(Device::with_capabilities(STREAMING_CAPABILITIES));
    device
        .borrow_mut()
        .iface
        .set_device_state(DeviceState::Healthy, RecoveryReason::NoFailure);
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Ended(Outcome::DeviceHealthy)
    );
    assert_eq!(agent.poll(), Served::Idle);

    // The task did not take the report of the first boot cycle. The next
    // post drops that end state, so no report carries it under the number
    // of the new boot cycle.
    *device.borrow_mut() = Device::awaiting_an_image();
    agent.post(BootCyclePost::Released {
        cycle: FIRST_CYCLE + 1,
    });
    assert_eq!(agent.take_report(), None);
    push_and_activate(&mut agent, &device);
    assert_eq!(
        staged_bytes(&device.borrow(), IMAGE_SIZES[0]),
        image_bytes(&images, 0)
    );
}

#[test]
fn a_failed_chunk_write_stops_the_sequence_and_the_next_sequence_announces_the_image_again() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    device.borrow_mut().write_fault = Some(WriteFault {
        command: RecoveryCommand::IndirectFifoData,
        passing: 1,
        arrives: true,
    });
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });

    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::Access(BusFault))
    );
    let writes_before = device.borrow().writes.len();
    assert_eq!(agent.sequence().phase(), Phase::ReadProtCap);

    push_and_activate(&mut agent, &device);
    // The new sequence clears RECOVERY_CTRL and announces the image, which
    // resets the FIFO, before the first chunk.
    let commands: Vec<RecoveryCommand> = device.borrow().writes[writes_before..]
        .iter()
        .map(|(command, _)| *command)
        .collect();
    assert_eq!(
        commands[..3],
        [
            RecoveryCommand::RecoveryCtrl,
            RecoveryCommand::IndirectFifoCtrl,
            RecoveryCommand::IndirectFifoData,
        ]
    );
    assert_eq!(
        staged_bytes(&device.borrow(), IMAGE_SIZES[0]),
        image_bytes(&images, 0)
    );
}

#[test]
fn a_refused_chunk_leads_to_a_restart_that_announces_the_image_again() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    device.borrow_mut().transport_max = DEVICE_MAX_TRANSFER / 2;
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });

    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::Protocol {
            command: RecoveryCommand::IndirectFifoData,
            code: ProtocolError::LengthWriteError as u8,
        })
    );

    device.borrow_mut().transport_max = DEVICE_MAX_TRANSFER;
    push_and_activate(&mut agent, &device);
    assert_eq!(
        device
            .borrow()
            .writes_to(RecoveryCommand::IndirectFifoCtrl)
            .len(),
        2
    );
    assert_eq!(
        staged_bytes(&device.borrow(), IMAGE_SIZES[0]),
        image_bytes(&images, 0)
    );
}

#[test]
fn a_missing_image_makes_the_agent_idle_and_reports_image_not_held() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    push_and_activate(&mut agent, &device);
    let missing = image_index(1);
    request_image(&mut device.borrow_mut(), missing);

    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::ImageNotHeld(missing))
    );
    assert_eq!(
        reports(&mut agent).last(),
        Some(&PushReport {
            cycle: FIRST_CYCLE,
            status: PushStatus::ImageNotHeld {
                index: missing.get()
            },
        })
    );
    let at_the_stop = accesses(&device);
    assert_eq!(agent.poll(), Served::Idle);
    assert_eq!(accesses(&device), at_the_stop);
}

#[test]
fn a_request_after_max_images_activations_gets_no_push_and_reports_image_limit() {
    let images = images(2);
    let device = RefCell::new(Device::awaiting_an_image());
    let limits = PushLimits {
        max_images: 1,
        ..LIMITS
    };
    let mut agent = agent(&device, &images, limits);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    push_and_activate(&mut agent, &device);
    let second = image_index(1);
    request_image(&mut device.borrow_mut(), second);

    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::ImageLimit(second))
    );
    assert_eq!(
        reports(&mut agent).last().map(|report| report.status),
        Some(PushStatus::ImageLimit)
    );
    assert_eq!(
        device
            .borrow()
            .writes_to(RecoveryCommand::IndirectFifoCtrl)
            .len(),
        1
    );
    assert_eq!(agent.poll(), Served::Idle);
}

#[test]
fn the_image_limit_counts_the_activations_before_a_restart() {
    let images = images(IMAGE_SIZES.len());
    let device = RefCell::new(Device::awaiting_an_image());
    let limits = PushLimits {
        max_images: 2,
        ..LIMITS
    };
    let mut agent = agent(&device, &images, limits);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    push_and_activate(&mut agent, &device);

    // The first chunk of the second image fails, so the agent restarts.
    request_image(&mut device.borrow_mut(), image_index(1));
    device.borrow_mut().write_fault = Some(WriteFault {
        command: RecoveryCommand::IndirectFifoData,
        passing: 0,
        arrives: false,
    });
    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::Access(BusFault))
    );
    push_and_activate(&mut agent, &device);
    assert_eq!(
        reports(&mut agent).last().map(|report| report.status),
        Some(PushStatus::NoEndState { activated: 2 })
    );

    let third = image_index(2);
    request_image(&mut device.borrow_mut(), third);
    assert_eq!(
        poll_until_it_rests(&mut agent),
        Served::Stopped(AgentError::ImageLimit(third))
    );
}

#[test]
fn after_max_restarts_the_agent_reports_restart_limit_and_makes_no_further_access() {
    let images = images(1);
    let device = RefCell::new(Device::awaiting_an_image());
    // Every readback of the announcement names another image size.
    device.borrow_mut().read_fault =
        Some((RecoveryCommand::IndirectFifoCtrl, ReadFault::OtherImageSize));
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });

    let sequences = LIMITS.max_restarts + 1;
    for _ in 0..sequences {
        assert!(matches!(
            poll_until_it_rests(&mut agent),
            Served::Stopped(AgentError::AnnouncementMismatch { .. })
        ));
    }
    assert_eq!(
        reports(&mut agent),
        [PushReport {
            cycle: FIRST_CYCLE,
            status: PushStatus::RestartLimit,
        }]
    );
    assert_eq!(
        device
            .borrow()
            .writes_to(RecoveryCommand::IndirectFifoCtrl)
            .len(),
        sequences as usize
    );
    let at_the_limit = accesses(&device);
    assert_eq!(agent.poll(), Served::Idle);
    assert_eq!(accesses(&device), at_the_limit);
}

#[test]
fn a_device_with_no_end_state_keeps_the_agent_waiting_and_the_reports_count_the_activations() {
    let images = images(2);
    let device = RefCell::new(Device::awaiting_an_image());
    let mut agent = agent(&device, &images, LIMITS);
    agent.post(BootCyclePost::Released { cycle: FIRST_CYCLE });
    // A post is no report.
    assert_eq!(agent.take_report(), None);

    push_and_activate(&mut agent, &device);
    assert_eq!(
        agent.take_report(),
        Some(PushReport {
            cycle: FIRST_CYCLE,
            status: PushStatus::NoEndState { activated: 1 },
        })
    );
    // `take_report` returns a report once per change of the status.
    assert_eq!(agent.take_report(), None);

    request_image(&mut device.borrow_mut(), image_index(1));
    push_and_activate(&mut agent, &device);
    device
        .borrow_mut()
        .iface
        .set_recovery_status(RecoveryState::BootingImage, image_index(1));
    assert_eq!(poll_until_it_rests(&mut agent), Served::Waiting);
    assert_eq!(
        reports(&mut agent),
        [PushReport {
            cycle: FIRST_CYCLE,
            status: PushStatus::NoEndState { activated: 2 },
        }]
    );
}
