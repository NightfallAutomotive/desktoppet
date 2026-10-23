//! The Linux input worker.
//!
//! One thread observes every keyboard and mouse node under `/dev/input` and
//! publishes edges, pointer motion and reconciliations in arrival order. The
//! protocol is the other platforms' (AGENTS.md §5):
//!
//! - key and button edges go through the reliable runtime queue; a full queue
//!   is counted and recovered with a `QueueOverflow` reset, never dropped
//!   silently;
//! - every [`RECONCILIATION_INTERVAL`], and at once after the kernel reports
//!   dropped events, the pressed set is re-read from the devices with
//!   `EVIOCGKEY`, so a release that was missed is cleared;
//! - a device that disappears resets the input state (`DeviceRemoved`);
//! - new devices are picked up every [`RESCAN_INTERVAL`].
//!
//! Wayland has no global cursor position, so the position the model follows is
//! accumulated from relative motion inside a viewport the overlay supplies
//! (ADR-0081, ADR-0090). Sleep, screen lock and VT or user switches reset the
//! input state through systemd-logind ([`SessionWatch`]).

use super::{
    decoder::{Decoded, Decoder, RawEvent, control_for},
    device::{InputDevice, OpenError, poll_readable},
    session_watch::SessionWatch,
};
use crate::PlatformInputError;
use bongocat_input::{
    CursorDelta, CursorMotionAccumulator, CursorPosition, CursorProducer, CursorSample,
    CursorViewport, GamepadAxisProducer, InputControl, InputEvent, InputProducer,
    InputPublishError, InputResetReason, InputSource, MonotonicMillis, PlatformInputDiagnostics,
    PlatformInputDiagnosticsProducer, PlatformInputServiceStatus,
};
use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INPUT_DIRECTORY: &str = "/dev/input";
pub(crate) const RECONCILIATION_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// How long one `poll` may wait, which bounds how long a stop request waits.
const POLL_TIMEOUT_MS: i32 = 50;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(2);

/// The viewport the cursor is accumulated in before the overlay names one.
pub const DEFAULT_CURSOR_VIEWPORT: CursorViewport = CursorViewport {
    origin: CursorPosition { x: 0.0, y: 0.0 },
    width: 1920.0,
    height: 1080.0,
};

pub struct LinuxInputService {
    stop: Arc<AtomicBool>,
    viewport: Arc<Mutex<CursorViewport>>,
    completion: Receiver<Result<PlatformInputDiagnostics, PlatformInputError>>,
    worker: Option<JoinHandle<()>>,
}

impl LinuxInputService {
    pub fn start_with_diagnostics(
        producer: InputProducer,
        cursor_producer: CursorProducer,
        gamepad_axis_producer: GamepadAxisProducer,
        diagnostics_producer: PlatformInputDiagnosticsProducer,
        viewport: CursorViewport,
    ) -> Result<Self, PlatformInputError> {
        // Gamepads come from the gilrs backend in a later step of stage L5.
        drop(gamepad_axis_producer);
        let stop = Arc::new(AtomicBool::new(false));
        let viewport = Arc::new(Mutex::new(viewport));
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let worker_stop = Arc::clone(&stop);
        let worker_viewport = Arc::clone(&viewport);
        let worker = thread::Builder::new()
            .name("bongocat-linux-input".into())
            .spawn(move || {
                let startup_on_panic = startup_sender.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    Worker::new(
                        producer,
                        cursor_producer,
                        diagnostics_producer,
                        worker_viewport,
                        worker_stop,
                    )
                    .run(Path::new(INPUT_DIRECTORY), &startup_sender)
                }))
                .unwrap_or(Err(PlatformInputError::WorkerPanicked));
                if let Err(error) = result {
                    let _ = startup_on_panic.try_send(Err(error));
                }
                let _ = completion_sender.send(result);
            })
            .map_err(|_| PlatformInputError::WorkerPanicked)?;
        match startup_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                stop,
                viewport,
                completion: completion_receiver,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(PlatformInputError::StartupTimedOut)
            }
        }
    }

    /// The area the cursor position is accumulated in: the overlay's output.
    pub fn set_viewport(&self, viewport: CursorViewport) {
        *self
            .viewport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = viewport;
    }

    /// Linux has no absolute cursor to fall back to, so the position is always
    /// accumulated from device motion, which is what `force_move` asks for on
    /// the other platforms. The setting therefore changes nothing here.
    pub fn set_force_move(&self, _enabled: bool) {}

    pub fn stop(mut self) -> Result<PlatformInputDiagnostics, PlatformInputError> {
        self.finish()
    }

    fn finish(&mut self) -> Result<PlatformInputDiagnostics, PlatformInputError> {
        self.stop.store(true, Ordering::Release);
        let result = self
            .completion
            .recv_timeout(SERVICE_TIMEOUT)
            .map_err(|_| PlatformInputError::ShutdownTimedOut)?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| PlatformInputError::WorkerPanicked)?;
        }
        result
    }
}

impl Drop for LinuxInputService {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.finish();
        }
    }
}

struct OpenDevice {
    device: InputDevice,
    decoder: Decoder,
}

/// Every keyboard and mouse node in `directory` that is not open yet.
/// `permission_denied` reports whether any candidate refused to open.
fn scan(directory: &Path, open: &[OpenDevice]) -> (Vec<OpenDevice>, bool) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return (Vec::new(), false);
    };
    let mut permission_denied = false;
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .filter(|path| open.iter().all(|device| device.device.path() != path))
        .collect::<Vec<PathBuf>>();
    paths.sort();
    let devices = paths
        .iter()
        .filter_map(|path| match InputDevice::open(path) {
            Ok(device) => Some(OpenDevice {
                decoder: device
                    .touchpad()
                    .map_or_else(Decoder::default, Decoder::for_touchpad),
                device,
            }),
            Err(OpenError::PermissionDenied) => {
                permission_denied = true;
                None
            }
            Err(OpenError::Skipped) => None,
        })
        .collect();
    (devices, permission_denied)
}

struct Worker {
    producer: InputProducer,
    cursor_producer: CursorProducer,
    diagnostics_producer: PlatformInputDiagnosticsProducer,
    viewport: Arc<Mutex<CursorViewport>>,
    stop: Arc<AtomicBool>,
    started: Instant,
    diagnostics: PlatformInputDiagnostics,
    accumulator: CursorMotionAccumulator,
    recovery_pending: bool,
}

/// What publishing an event achieved.
enum Published {
    Delivered,
    /// The runtime queue was full; a recovery reset is now pending.
    Overflowed,
    RuntimeStopped,
}

impl Worker {
    fn new(
        producer: InputProducer,
        cursor_producer: CursorProducer,
        diagnostics_producer: PlatformInputDiagnosticsProducer,
        viewport: Arc<Mutex<CursorViewport>>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            producer,
            cursor_producer,
            diagnostics_producer,
            viewport,
            stop,
            started: Instant::now(),
            diagnostics: PlatformInputDiagnostics {
                service_start_attempts: 1,
                ..PlatformInputDiagnostics::default()
            },
            accumulator: CursorMotionAccumulator::default(),
            recovery_pending: false,
        }
    }

    fn now(&self) -> MonotonicMillis {
        MonotonicMillis::new(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn run(
        mut self,
        directory: &Path,
        startup: &SyncSender<Result<(), PlatformInputError>>,
    ) -> Result<PlatformInputDiagnostics, PlatformInputError> {
        if !directory.is_dir() {
            return Err(PlatformInputError::BackendUnavailable);
        }
        let (mut devices, permission_denied) = scan(directory, &[]);
        if devices.is_empty() {
            return Err(if permission_denied {
                PlatformInputError::PermissionDenied
            } else {
                PlatformInputError::BackendUnavailable
            });
        }
        self.diagnostics.service_status = PlatformInputServiceStatus::Running;
        self.publish_diagnostics();
        let _ = startup.send(Ok(()));

        let session_watch = SessionWatch::start();
        let mut last_reconcile = Instant::now();
        let mut last_scan = Instant::now();
        let mut events = Vec::<RawEvent>::new();
        while !self.stop.load(Ordering::Acquire) {
            let fds = devices
                .iter()
                .map(|device| device.device.raw_fd())
                .collect::<Vec<_>>();
            let readable = poll_readable(&fds, POLL_TIMEOUT_MS)
                .map_err(|_| PlatformInputError::WorkerPanicked)?;
            let mut reconcile_now = false;
            let mut removed = Vec::new();
            let mut motion = CursorDelta::default();
            for index in readable {
                let device = &mut devices[index];
                events.clear();
                if device.device.read_events(&mut events).is_err() {
                    removed.push(index);
                }
                for event in events.drain(..) {
                    match device.decoder.decode(event) {
                        Some(Decoded::Edge { control, edge }) => {
                            self.diagnostics.captured_edges =
                                self.diagnostics.captured_edges.saturating_add(1);
                            let at = self.now();
                            match self.publish(InputEvent::Edge {
                                control,
                                edge,
                                source: InputSource::Capture,
                                at,
                            }) {
                                Published::RuntimeStopped => return Ok(self.stopped()),
                                Published::Delivered => {
                                    self.diagnostics.queued_edges =
                                        self.diagnostics.queued_edges.saturating_add(1);
                                }
                                Published::Overflowed => {}
                            }
                        }
                        Some(Decoded::Motion { dx, dy }) => {
                            motion.x += dx;
                            motion.y += dy;
                        }
                        Some(Decoded::Resync) => {
                            self.diagnostics.decode_errors =
                                self.diagnostics.decode_errors.saturating_add(1);
                            reconcile_now = true;
                        }
                        Some(Decoded::Unmapped) => {
                            self.diagnostics.unmapped_keys =
                                self.diagnostics.unmapped_keys.saturating_add(1);
                        }
                        None => {}
                    }
                }
            }
            if !removed.is_empty() {
                for index in removed.into_iter().rev() {
                    devices.remove(index);
                }
                // Whatever the removed device held down can never be released
                // by it, so the whole pressed state starts over.
                if let Published::RuntimeStopped = self.reset(InputResetReason::DeviceRemoved) {
                    return Ok(self.stopped());
                }
            }
            for reason in session_watch.take_resets() {
                if let Published::RuntimeStopped = self.reset(reason) {
                    return Ok(self.stopped());
                }
            }
            if motion != CursorDelta::default() {
                self.publish_cursor(motion);
            }
            if self.recovery_pending
                && let Published::RuntimeStopped = self.reset(InputResetReason::QueueOverflow)
            {
                return Ok(self.stopped());
            }
            if reconcile_now || last_reconcile.elapsed() >= RECONCILIATION_INTERVAL {
                last_reconcile = Instant::now();
                if let Published::RuntimeStopped = self.reconcile(&devices) {
                    return Ok(self.stopped());
                }
                self.publish_diagnostics();
            }
            if last_scan.elapsed() >= RESCAN_INTERVAL {
                last_scan = Instant::now();
                let (added, _) = scan(directory, &devices);
                devices.extend(added);
            }
        }
        Ok(self.stopped())
    }

    fn publish(&mut self, event: InputEvent) -> Published {
        match self.producer.publish(event) {
            Ok(_) => Published::Delivered,
            Err(InputPublishError::QueueFull(_)) => {
                self.diagnostics.runtime_queue_overflows =
                    self.diagnostics.runtime_queue_overflows.saturating_add(1);
                self.recovery_pending = true;
                Published::Overflowed
            }
            Err(InputPublishError::RuntimeStopped(_)) => Published::RuntimeStopped,
        }
    }

    fn reset(&mut self, reason: InputResetReason) -> Published {
        let at = self.now();
        self.accumulator.reset();
        match self.producer.recover(reason, at) {
            Ok(_) => {
                self.recovery_pending = false;
                self.diagnostics.recovery_resets =
                    self.diagnostics.recovery_resets.saturating_add(1);
                Published::Delivered
            }
            Err(InputPublishError::QueueFull(_)) => {
                self.recovery_pending = true;
                Published::Overflowed
            }
            Err(InputPublishError::RuntimeStopped(_)) => Published::RuntimeStopped,
        }
    }

    fn reconcile(&mut self, devices: &[OpenDevice]) -> Published {
        let pressed = devices
            .iter()
            .filter_map(|device| device.device.pressed_codes().ok())
            .flatten()
            .filter_map(control_for)
            .collect::<BTreeSet<InputControl>>();
        let at = self.now();
        let published = self.publish(InputEvent::Reconcile { pressed, at });
        if matches!(published, Published::Delivered) {
            self.diagnostics.reconciliation_runs =
                self.diagnostics.reconciliation_runs.saturating_add(1);
        }
        published
    }

    fn publish_cursor(&mut self, delta: CursorDelta) {
        let viewport = *self
            .viewport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // There is no absolute position to seed from, so a fresh accumulation
        // starts at the middle of the viewport.
        let center = CursorPosition {
            x: viewport.origin.x + viewport.width / 2.0,
            y: viewport.origin.y + viewport.height / 2.0,
        };
        let position = self.accumulator.advance(delta, center, viewport);
        self.diagnostics.cursor_captured = self.diagnostics.cursor_captured.saturating_add(1);
        match CursorSample::following(position, position, viewport, self.now()) {
            Ok(sample) => {
                if self.cursor_producer.publish(sample).is_err() {
                    self.diagnostics.cursor_publish_rejections =
                        self.diagnostics.cursor_publish_rejections.saturating_add(1);
                }
            }
            Err(_) => {
                self.diagnostics.cursor_display_lookup_failures = self
                    .diagnostics
                    .cursor_display_lookup_failures
                    .saturating_add(1);
            }
        }
    }

    fn publish_diagnostics(&self) {
        let _ = self.diagnostics_producer.publish(self.diagnostics);
    }

    fn stopped(mut self) -> PlatformInputDiagnostics {
        self.diagnostics.service_status = PlatformInputServiceStatus::Stopped;
        self.diagnostics.clean_shutdown = true;
        self.publish_diagnostics();
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn start_in(directory: &Path) -> Result<(), PlatformInputError> {
        let worker = Worker::new(
            InputProducer::new(Arc::new(NullSubmitter)),
            CursorProducer::new(),
            PlatformInputDiagnosticsProducer::default(),
            Arc::new(Mutex::new(DEFAULT_CURSOR_VIEWPORT)),
            Arc::new(AtomicBool::new(true)),
        );
        let (startup, _receiver) = mpsc::sync_channel(1);
        worker.run(directory, &startup).map(|_| ())
    }

    struct NullSubmitter;

    impl bongocat_input::InputSubmitter for NullSubmitter {
        fn submit(
            &self,
            _: bongocat_input::SequencedInputEvent,
        ) -> Result<(), bongocat_input::InputSubmitError> {
            Ok(())
        }
    }

    /// Which of this machine's input nodes the service opens, and which of
    /// them it treats as touchpads. Prints no key data.
    #[test]
    #[ignore = "needs read access to /dev/input"]
    fn list_the_devices_this_machine_offers() {
        let (devices, permission_denied) = scan(Path::new(INPUT_DIRECTORY), &[]);
        assert!(
            !permission_denied || !devices.is_empty(),
            "no access to /dev/input"
        );
        for device in &devices {
            eprintln!(
                "{}: touchpad scale {:?}",
                device.device.path().display(),
                device.device.touchpad()
            );
        }
        assert!(!devices.is_empty());
    }

    #[test]
    fn a_missing_input_directory_is_an_unavailable_backend() {
        let directory = tempfile::tempdir().expect("temporary directory");
        assert_eq!(
            start_in(&directory.path().join("missing")),
            Err(PlatformInputError::BackendUnavailable)
        );
    }

    #[test]
    fn nodes_that_are_not_keyboards_or_mice_are_an_unavailable_backend() {
        let directory = tempfile::tempdir().expect("temporary directory");
        std::fs::write(directory.path().join("event0"), b"").expect("plain file");
        assert_eq!(
            start_in(directory.path()),
            Err(PlatformInputError::BackendUnavailable)
        );
    }

    #[test]
    fn nodes_that_refuse_to_open_are_a_permission_problem() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let node = directory.path().join("event0");
        std::fs::write(&node, b"").expect("node");
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000))
            .expect("make unreadable");
        if std::fs::File::open(&node).is_ok() {
            // Running as root: permissions do not apply, so there is nothing to
            // observe.
            return;
        }
        assert_eq!(
            start_in(directory.path()),
            Err(PlatformInputError::PermissionDenied)
        );
    }
}
