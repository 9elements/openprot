// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The orchestrator side of the Recovery Agent with the platform driver and
//! the checkpoint walk of the orchestrator, both unmodified.
//!
//! The test plays 2 loops in turn. The orchestrator loop part calls the
//! driver and hands the reports to the [`BootCycle`]. The Recovery Agent
//! task part takes the post, polls the agent against the device dispatch
//! and sets the device states the downstream device would set (recovery
//! pending, then healthy).

use core::cell::{Cell, RefCell};
use core::convert::Infallible;
use core::marker::PhantomData;
use core::time::Duration;

use openprot_ocp_recovery_agent::{
    BootCyclePost, Outcome, Phase, PushLimits, PushReport, PushStatus, RecoveryAgent,
    RegisterAccess, Served,
};
use openprot_ocp_recovery_api::regs::{DeviceState, ImageSelection, RecoveryReason};
use openprot_ocp_recovery_api::RecoveryCommand;
use openprot_orchestrator_driver::{
    Board, BoardCapabilities, ImageSource, PlatformDriver, SvnFloorBinding, Verdict, Verifier,
};
use openprot_orchestrator_ocp_adapter::{BootCycle, BootCyclePostSink, NotifyingBootControl};
use openprot_orchestrator_sm::{BootFailureKind, ComponentId, ComponentKind, Event};
use orchestrator_capabilities::{BootControl, BootStatus, EvidenceReader, Svn, SvnFloor};
use orchestrator_checkpoint_walk::CheckpointWalk;
use orchestrator_config::{BootCheckpoint, DeviceConfig};

use ocp_recovery_test_fixture::{
    staged_bytes, BusFault, Device, Held, ImageList, SliceImage, CODE_CMS, DEVICE_MAX_TRANSFER,
    IMAGE_SIZES, STEP_LIMIT,
};

/// The downstream device is component 0 of the board.
const NIC: ComponentId = ComponentId::new(0);

/// The name of the checkpoint that the push report answers.
const PUSH_CHECKPOINT: &str = "ocp-push";

/// The window the board gives the push.
const PUSH_WINDOW: Duration = Duration::from_secs(30);

/// The probe vocabulary of the test board.
#[derive(Debug)]
enum Probe {
    /// The report of the Recovery Agent task.
    Push,
}

static NIC_CONFIG: DeviceConfig<(), Probe> = DeviceConfig::new(
    "nic",
    (),
    &[BootCheckpoint::new(
        PUSH_CHECKPOINT,
        Probe::Push,
        PUSH_WINDOW,
    )],
);

const LIMITS: PushLimits = PushLimits {
    max_images: 1,
    max_restarts: 0,
};

/// The status of a push that the downstream device ended with a boot.
const SUCCESS: PushStatus = PushStatus::Ended(Outcome::Success);

/// The reset line of the downstream device. A release boots the device into
/// recovery mode, where it requests the first image.
struct ResetLine<'a>(&'a RefCell<Device>);

impl BootControl for ResetLine<'_> {
    type Error = Infallible;

    fn hold_in_reset(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn release(&mut self) -> Result<(), Infallible> {
        *self.0.borrow_mut() = Device::awaiting_an_image();
        Ok(())
    }
}

/// The post on the way to the Recovery Agent task. A newer post replaces a
/// post the task has not taken.
struct PostLatch<'a>(&'a Cell<Option<BootCyclePost>>);

impl BootCyclePostSink for PostLatch<'_> {
    fn post(&mut self, post: BootCyclePost) {
        self.0.set(Some(post));
    }
}

/// The boot evidence of the downstream device: the report the orchestrator
/// side kept.
struct PushEvidence<'a>(&'a BootCycle);

impl EvidenceReader<Probe> for PushEvidence<'_> {
    type Error = Infallible;

    fn read(&mut self, probe: &Probe) -> Result<BootStatus, Infallible> {
        match probe {
            Probe::Push => Ok(self.0.boot_status()),
        }
    }
}

/// The driver requires an image and a verifier for each component. The
/// tests call `release_reset` and `poll_boot_walks` only, so the driver
/// runs neither stub.
struct NoImage;

impl ImageSource for NoImage {
    type Error = Infallible;

    fn open(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn size(&self) -> Result<usize, Infallible> {
        Ok(0)
    }

    fn read_at(&mut self, _offset: usize, _buf: &mut [u8]) -> Result<(), Infallible> {
        Ok(())
    }
}

struct NoVerifier;

impl Verifier for NoVerifier {
    type Error = Infallible;

    fn verify(
        &mut self,
        _id: ComponentId,
        _image: &mut impl ImageSource,
    ) -> Result<Verdict, Infallible> {
        Ok(Verdict::Rejected)
    }
}

/// The downstream device keeps its SVN floor, so the board has none.
struct NoFloor;

impl SvnFloor for NoFloor {
    type Error = Infallible;

    fn floor(&self) -> Result<Svn, Infallible> {
        Ok(Svn(0))
    }

    fn advance(&mut self, _to: Svn) -> Result<(), Infallible> {
        Ok(())
    }
}

/// The type choices of the test board. `BootControl` and `BootWatch` borrow
/// the `BootCycle` of the downstream device.
struct TestBoard<'a>(PhantomData<&'a ()>);

impl<'a> BoardCapabilities for TestBoard<'a> {
    type Image = NoImage;
    type Verifier = NoVerifier;
    type BootControl = NotifyingBootControl<'a, ResetLine<'a>, PostLatch<'a>>;
    type BootWatch = CheckpointWalk<PushEvidence<'a>, Probe>;
    type SvnFloor = NoFloor;
    type ReportSink = ();
}

fn driver<'a>(
    device: &'a RefCell<Device>,
    cycle: &'a BootCycle,
    post: &'a Cell<Option<BootCyclePost>>,
) -> PlatformDriver<TestBoard<'a>, 1> {
    PlatformDriver::new(Board {
        images: [NoImage],
        verifier: NoVerifier,
        boot_controls: [NotifyingBootControl::new(
            ResetLine(device),
            cycle,
            PostLatch(post),
        )],
        boot_watches: [CheckpointWalk::new(PushEvidence(cycle), &NIC_CONFIG)],
        component_kinds: [ComponentKind::Passive],
        svn_floors: [SvnFloorBinding::SelfManaged],
        report_sink: (),
    })
}

/// The register access of the Recovery Agent to the device behind
/// `dispatch`.
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

fn first_image() -> ImageList {
    ImageList(vec![Held::Image(SliceImage::pattern(IMAGE_SIZES[0]))])
}

/// Runs the Recovery Agent task until the agent is idle: the task takes
/// the post, polls, sets the device states the downstream device would
/// set, and hands every report to `intake`. `intake` stands for the
/// orchestrator loop, which calls `BootCycle::accept` when a report wakes
/// the loop.
fn run_task(
    agent: &mut Agent<'_>,
    device: &RefCell<Device>,
    post: &Cell<Option<BootCyclePost>>,
    mut intake: impl FnMut(PushReport),
) {
    if let Some(post) = post.take() {
        agent.post(post);
    }
    for _ in 0..STEP_LIMIT {
        match agent.poll() {
            Served::Advanced => {}
            Served::Waiting => match agent.sequence().phase() {
                Phase::AwaitRecoveryPending => device.borrow_mut().iface.set_device_state(
                    DeviceState::RecoveryPending,
                    RecoveryReason::FlashlessStreamingBoot,
                ),
                // The activation is with the device, which boots the image
                // and reports healthy.
                Phase::PollRecoveryStatus => device
                    .borrow_mut()
                    .iface
                    .set_device_state(DeviceState::Healthy, RecoveryReason::NoFailure),
                phase => panic!("the push waits in {phase:?}"),
            },
            Served::Idle | Served::Ended(_) => {
                while let Some(report) = agent.take_report() {
                    intake(report);
                }
                return;
            }
            other => panic!("the push stopped: {other:?}"),
        }
        while let Some(report) = agent.take_report() {
            intake(report);
        }
    }
    panic!("the task did not finish within {STEP_LIMIT} polls");
}

#[test]
fn the_walk_completes_when_the_loop_polls_the_walks_right_after_the_report() {
    let device = RefCell::new(Device::awaiting_an_image());
    let cycle = BootCycle::new();
    let post = Cell::new(None);
    let images = first_image();
    let mut agent: Agent<'_> = RecoveryAgent::new(
        Bus(&device),
        &images,
        CODE_CMS,
        ImageSelection::None,
        LIMITS,
    );
    let mut driver = driver(&device, &cycle, &post);

    driver.release_reset(NIC).expect("the reset line moves");
    // The push has not begun, so the walk waits.
    let poll = driver.poll_boot_walks(0);
    assert_eq!(poll.event, None);
    assert_eq!(
        poll.next_deadline_millis,
        Some(PUSH_WINDOW.as_millis() as u64)
    );

    run_task(&mut agent, &device, &post, |report| cycle.accept(report));

    assert_eq!(cycle.kept_status(), Some(SUCCESS));
    let Held::Image(image) = &images.0[0] else {
        panic!("the test list holds the first image");
    };
    assert_eq!(staged_bytes(&device.borrow(), IMAGE_SIZES[0]), image.0);
    assert_eq!(driver.poll_boot_walks(1).event, Some(Event::Booted(NIC)));
}

#[test]
fn the_walk_fails_with_a_timeout_when_no_report_arrives() {
    let device = RefCell::new(Device::awaiting_an_image());
    let cycle = BootCycle::new();
    let post = Cell::new(None);
    let mut driver = driver(&device, &cycle, &post);

    driver.release_reset(NIC).expect("the reset line moves");
    assert_eq!(driver.poll_boot_walks(0).event, None);
    // No task takes the post, so no report arrives in the window.
    assert_eq!(
        driver.poll_boot_walks(PUSH_WINDOW.as_millis() as u64).event,
        Some(Event::BootFailed {
            id: NIC,
            checkpoint: PUSH_CHECKPOINT,
            kind: BootFailureKind::TimedOut,
        })
    );
}

#[test]
fn a_success_of_the_boot_cycle_before_reads_as_booting_after_the_next_release() {
    let device = RefCell::new(Device::awaiting_an_image());
    let cycle = BootCycle::new();
    let post = Cell::new(None);
    let images = first_image();
    let mut agent: Agent<'_> = RecoveryAgent::new(
        Bus(&device),
        &images,
        CODE_CMS,
        ImageSelection::None,
        LIMITS,
    );
    let mut driver = driver(&device, &cycle, &post);

    driver.release_reset(NIC).expect("the reset line moves");
    assert_eq!(driver.poll_boot_walks(0).event, None);
    let mut last = None;
    run_task(&mut agent, &device, &post, |report| {
        cycle.accept(report);
        last = Some(report);
    });
    assert_eq!(driver.poll_boot_walks(1).event, Some(Event::Booted(NIC)));
    let success = last.expect("the task reported the end state");
    assert_eq!(success.status, SUCCESS);

    // The driver releases the device again, and the task has not taken the
    // post yet. The walk returns no event, and a late copy of the report of
    // the boot cycle before does not change the answer.
    driver.release_reset(NIC).expect("the reset line moves");
    assert_eq!(driver.poll_boot_walks(2).event, None);
    cycle.accept(success);
    assert_eq!(driver.poll_boot_walks(3).event, None);

    // The task takes the post and pushes again, and the walk completes.
    run_task(&mut agent, &device, &post, |report| cycle.accept(report));
    assert_eq!(driver.poll_boot_walks(4).event, Some(Event::Booted(NIC)));
}
