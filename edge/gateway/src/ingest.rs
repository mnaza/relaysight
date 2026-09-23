//! What is being pushed to this gateway, whatever protocol brought it.
//!
//! A publisher arrives on a stream key the dashboard listed as a source, and
//! its frames go to whoever is watching or recording. RTMP feeds this, and SRT
//! does too when that feature is on; neither knows about the other.
//! See docs/superpowers/specs/2026-09-22-video-sources-design.md.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::broadcast;

use crate::frames::{Frame, FrameSource, VideoParameters};

/// Enough to hold a second or two of frames for a consumer that stalls.
const FRAME_BUFFER: usize = 120;

/// What the gateway accepts, and what is arriving.
#[derive(Clone, Default)]
pub struct Ingest {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    allowed: HashSet<String>,
    streams: HashMap<String, Stream>,
}

pub(crate) struct Stream {
    frames: broadcast::Sender<Frame>,
    state: Arc<Mutex<StreamState>>,
}

#[derive(Default)]
pub(crate) struct StreamState {
    pub(crate) avcc: Option<Vec<u8>>,
    pub(crate) codec: Option<String>,
    pub(crate) dimensions: Option<(u32, u32)>,
    pub(crate) frame_rate: Option<(u32, u32)>,
    pub(crate) last_frame: Option<Instant>,
    pub(crate) frames: u64,
    pub(crate) bytes: u64,
}

impl StreamState {
    fn parameters(&self) -> Option<VideoParameters> {
        // A recorder needs both, and a publisher may send them in either order
        // — or send no metadata at all, in which case there is nothing honest
        // to report and live view still works.
        let (avcc, codec, dimensions) =
            (self.avcc.as_ref()?, self.codec.as_ref()?, self.dimensions?);
        Some(VideoParameters {
            rfc6381_codec: codec.clone(),
            pixel_dimensions: dimensions,
            extra_data: avcc.clone(),
            frame_rate: self.frame_rate,
        })
    }
}

/// What a pushed stream is doing, for telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestStats {
    pub frames: u64,
    pub bytes: u64,
    pub publishing: bool,
    /// What the publisher said it is sending, once it has said anything.
    pub codec: Option<String>,
    pub dimensions: Option<(u32, u32)>,
}

impl Ingest {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stream keys the control plane has listed as sources. Replaces the
    /// set, so removing a source stops the next publisher using its key.
    pub fn allow<I: IntoIterator<Item = String>>(&self, keys: I) {
        let mut inner = self.lock();
        inner.allowed = keys.into_iter().collect();
    }

    pub fn is_allowed(&self, key: &str) -> bool {
        self.lock().allowed.contains(key)
    }

    /// Frames from a publisher on this key, if one has published. The source
    /// ends when the publisher does.
    pub fn subscribe(&self, key: &str) -> Option<PushedSource> {
        let inner = self.lock();
        let stream = inner.streams.get(key)?;
        Some(PushedSource {
            frames: stream.frames.subscribe(),
            state: Arc::clone(&stream.state),
            key: key.to_owned(),
        })
    }

    /// `None` when nobody has ever published on this key.
    pub fn stats(&self, key: &str, fresh_within: Duration) -> Option<IngestStats> {
        let inner = self.lock();
        let state = inner.streams.get(key)?.state.lock().ok()?;
        Some(IngestStats {
            frames: state.frames,
            bytes: state.bytes,
            publishing: state
                .last_frame
                .is_some_and(|at| at.elapsed() < fresh_within),
            codec: state.codec.clone(),
            dimensions: state.dimensions,
        })
    }

    pub(crate) fn open(&self, key: &str) -> Arc<Mutex<StreamState>> {
        let mut inner = self.lock();
        let stream = inner
            .streams
            .entry(key.to_owned())
            .or_insert_with(|| Stream {
                frames: broadcast::channel(FRAME_BUFFER).0,
                state: Arc::new(Mutex::new(StreamState::default())),
            });
        Arc::clone(&stream.state)
    }

    /// Dropping the sender is how consumers learn the publisher left.
    pub(crate) fn close(&self, key: &str) {
        self.lock().streams.remove(key);
    }

    pub(crate) fn publish(&self, key: &str, frame: Frame) {
        let inner = self.lock();
        if let Some(stream) = inner.streams.get(key) {
            // An error here means nobody is watching or recording, which is the
            // normal state of an unwatched camera.
            let _ = stream.frames.send(frame);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One consumer's view of a pushed stream.
pub struct PushedSource {
    frames: broadcast::Receiver<Frame>,
    state: Arc<Mutex<StreamState>>,
    key: String,
}

#[async_trait::async_trait]
impl FrameSource for PushedSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
        loop {
            match self.frames.recv().await {
                Ok(frame) => return Ok(Some(frame)),
                // This consumer fell behind the publisher. Skipping ahead beats
                // ending the recording, and the next keyframe repairs it.
                Err(broadcast::error::RecvError::Lagged(frames)) => {
                    tracing::warn!(key = %self.key, frames, "pushed stream consumer fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }

    fn parameters(&self) -> Option<VideoParameters> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .parameters()
    }
}

pub(crate) fn lock(state: &Arc<Mutex<StreamState>>) -> std::sync::MutexGuard<'_, StreamState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
