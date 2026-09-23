//! Who is being recorded right now, and keeping that in step with the
//! policies the control plane hands out.
//!
//! A camera set to record continuously gets a task of its own: open the
//! source, feed the ring buffer, and when that ends — a camera rebooting, a
//! publisher leaving — wait and open it again. A camera taken off continuous
//! recording has its task stopped, and one that disappears from the roster
//! does too.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::Notify;
use tracing::{info, warn};

use crate::{CameraSource, Config, frames::FrameSource, ringbuffer::Ring};

/// How long to wait before opening a source again after it ended or failed.
/// Long enough not to hammer a camera that is rebooting, short enough that a
/// blink costs a few seconds of ring rather than minutes.
const REOPEN_DELAY: Duration = Duration::from_secs(5);

struct Running {
    /// What the task was started for. A source whose address or credentials
    /// changed needs a new task, not the old one.
    source: CameraSource,
    stop: Arc<Notify>,
    handle: tokio::task::JoinHandle<()>,
}

/// The set of cameras this gateway is recording continuously.
#[derive(Default)]
pub struct Recorders {
    running: HashMap<String, Running>,
}

impl Recorders {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn recording(&self) -> usize {
        self.running.len()
    }

    #[cfg(test)]
    pub fn is_recording(&self, camera_id: &str) -> bool {
        self.running.contains_key(camera_id)
    }

    /// Start, stop and restart tasks until what is running is what was asked
    /// for. Called on every pass, so it has to be cheap when nothing changed.
    pub fn reconcile(&mut self, wanted: &[(String, CameraSource)], config: &Config) {
        let wanted: HashMap<&String, &CameraSource> =
            wanted.iter().map(|(id, source)| (id, source)).collect();

        self.running.retain(|camera_id, running| {
            let keep = match wanted.get(camera_id) {
                // A camera that moved address is a different thing to record.
                Some(source) => {
                    same_source(&running.source, source) && !running.handle.is_finished()
                }
                None => false,
            };
            if !keep {
                running.stop.notify_waiters();
                running.handle.abort();
                info!(camera_id, "stopped recording continuously");
            }
            keep
        });

        for (camera_id, source) in wanted {
            if self.running.contains_key(camera_id) {
                continue;
            }
            match self.start(camera_id.clone(), source.clone(), config) {
                Ok(running) => {
                    self.running.insert(camera_id.clone(), running);
                    info!(camera_id, "recording continuously");
                }
                Err(error) => warn!(camera_id, %error, "could not start recording"),
            }
        }
    }

    /// Stop everything, for a gateway shutting down.
    fn stop_all(&mut self) {
        for (_, running) in self.running.drain() {
            running.stop.notify_waiters();
            running.handle.abort();
        }
    }

    fn start(
        &self,
        camera_id: String,
        source: CameraSource,
        config: &Config,
    ) -> anyhow::Result<Running> {
        let ring = Ring::open(
            &config.recording_dir(),
            &camera_id,
            config.recording_budget_bytes,
        )?;
        let stop = Arc::new(Notify::new());
        let waiting = Arc::clone(&stop);
        let ingest = config.ingest.clone();
        let task_source = source.clone();
        let task_camera = camera_id.clone();
        let handle = tokio::spawn(async move {
            loop {
                match open(&task_source, &ingest).await {
                    Ok(mut frames) => {
                        if let Err(error) = ring.record(frames.as_mut(), &waiting).await {
                            warn!(camera_id = %task_camera, %error, "continuous recording ended");
                        }
                        // How much video this camera can still be asked for,
                        // which is the number an operator wants in the journal
                        // when a clip comes back shorter than they expected.
                        if let Ok(Some((from, to))) = ring.span() {
                            info!(
                                camera_id = %task_camera,
                                seconds = (to - from).num_seconds(),
                                "the ring holds this much"
                            );
                        }
                    }
                    Err(error) => {
                        warn!(camera_id = %task_camera, %error, "could not open a source to record");
                    }
                }
                // Either the source ended or it never opened. Wait, unless we
                // are being stopped, in which case leave now.
                tokio::select! {
                    _ = waiting.notified() => break,
                    _ = tokio::time::sleep(REOPEN_DELAY) => {}
                }
            }
        });
        Ok(Running {
            source,
            stop,
            handle,
        })
    }
}

impl Drop for Recorders {
    /// A gateway that stops reconciling stops recording: leaving tasks writing
    /// into a ring nobody is pruning is how a disk fills.
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Two sources are the same thing to record when they are the same stream with
/// the same way in.
fn same_source(one: &CameraSource, other: &CameraSource) -> bool {
    one.rtsp_uri == other.rtsp_uri
        && one.push_key == other.push_key
        && one.username == other.username
        && one.password == other.password
}

async fn open(
    source: &CameraSource,
    ingest: &crate::ingest::Ingest,
) -> anyhow::Result<Box<dyn FrameSource>> {
    match &source.push_key {
        // Already arriving: nothing to dial, and nothing to wait for but a
        // publisher.
        Some(key) => Ok(Box::new(ingest.subscribe(key).ok_or_else(|| {
            anyhow::anyhow!("nothing is publishing to stream key {key}")
        })?)),
        None => Ok(Box::new(
            crate::frames::RtspSource::open(
                &source.rtsp_uri,
                source.username.as_deref(),
                source.password.as_deref(),
                // The recorder's framing: four-byte lengths, parameters out of
                // band, which is what fMP4 stores.
                crate::frames::Framing::Avcc,
            )
            .await?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(address: &str) -> CameraSource {
        CameraSource {
            rtsp_uri: address.to_owned(),
            push_key: None,
            live_rtsp_uri: address.to_owned(),
            snapshot_uri: None,
            username: None,
            password: None,
        }
    }

    fn config(dir: &std::path::Path) -> Config {
        let mut config = crate::tests::config("http://127.0.0.1:1");
        config.recording_dir = Some(dir.to_path_buf());
        config
    }

    #[tokio::test]
    async fn what_is_running_follows_what_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut recorders = Recorders::new();

        // Nothing asked for, nothing running.
        recorders.reconcile(&[], &config);
        assert_eq!(recorders.recording(), 0);

        // Two cameras on continuous: two tasks, and the ring directories to
        // go with them.
        recorders.reconcile(
            &[
                ("cam-1".into(), source("rtsp://10.0.0.1/stream")),
                ("cam-2".into(), source("rtsp://10.0.0.2/stream")),
            ],
            &config,
        );
        assert_eq!(recorders.recording(), 2);
        assert!(recorders.is_recording("cam-1"));
        assert!(dir.path().join("cam-1").is_dir());

        // One taken off: its task goes, the other is left alone.
        recorders.reconcile(
            &[("cam-2".into(), source("rtsp://10.0.0.2/stream"))],
            &config,
        );
        assert_eq!(recorders.recording(), 1);
        assert!(!recorders.is_recording("cam-1"));
        assert!(recorders.is_recording("cam-2"));

        recorders.stop_all();
        assert_eq!(recorders.recording(), 0);
    }

    #[tokio::test]
    async fn a_camera_that_moved_is_recorded_from_its_new_address() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut recorders = Recorders::new();
        recorders.reconcile(
            &[("cam-1".into(), source("rtsp://10.0.0.1/stream"))],
            &config,
        );
        let first = recorders.running["cam-1"].source.rtsp_uri.clone();

        recorders.reconcile(
            &[("cam-1".into(), source("rtsp://10.0.0.9/stream"))],
            &config,
        );
        let second = recorders.running["cam-1"].source.rtsp_uri.clone();
        assert_ne!(first, second, "the old task was dialling the old address");
        assert_eq!(recorders.recording(), 1);
        recorders.stop_all();
    }

    #[tokio::test]
    async fn a_camera_id_that_could_name_a_directory_is_refused_rather_than_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut recorders = Recorders::new();
        recorders.reconcile(
            &[("../escape".into(), source("rtsp://10.0.0.1/s"))],
            &config,
        );
        assert_eq!(recorders.recording(), 0, "nothing started");
    }
}
