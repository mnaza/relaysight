//! What the gateway keeps on its own disk, so the minutes before something
//! happened are still there when someone asks.
//!
//! A camera with a continuous policy is recorded the whole time into a ring
//! buffer here, and only what a schedule, an operator, a plugin or an incident
//! asks for is ever uploaded. Uploading everything would cost roughly 650 GB a
//! month per 2 Mbit/s stream whether or not anyone watches it, which is the
//! same argument that keeps ingest on the gateway.
//!
//! **The directory is the index.** Every segment's name carries its start, its
//! length and which init segment decodes it, so a gateway that is killed
//! mid-write recovers by reading the directory rather than by trusting a file
//! it may not have finished writing.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, anyhow};
use chrono::{DateTime, TimeZone, Utc};

use crate::{
    frames::FrameSource,
    segmenter::{Pushed, Segmenter, ticks_to_ms},
};

/// How much video is cut into one file. Short enough that a clip does not
/// carry much it was not asked for, long enough that a day is not a million
/// files.
const SEGMENT_DURATION: Duration = Duration::from_secs(4);

/// One media segment on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSegment {
    pub start: DateTime<Utc>,
    pub duration_ms: u64,
    /// Which init segment decodes it. A stream that changes shape gets a new
    /// one, and a clip may never span two.
    pub init: String,
    pub path: PathBuf,
}

impl StoredSegment {
    pub fn end(&self) -> DateTime<Utc> {
        self.start + chrono::Duration::milliseconds(self.duration_ms as i64)
    }

    fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.m4s",
            self.start.timestamp_millis(),
            self.duration_ms,
            self.init
        )
    }

    /// Read back what the name says. `None` for anything this did not write.
    fn parse(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.strip_suffix(".m4s")?;
        let mut parts = name.splitn(3, '-');
        let start_ms: i64 = parts.next()?.parse().ok()?;
        let duration_ms: u64 = parts.next()?.parse().ok()?;
        let init = parts.next()?.to_owned();
        if init.is_empty() {
            return None;
        }
        Some(Self {
            start: Utc.timestamp_millis_opt(start_ms).single()?,
            duration_ms,
            init,
            path: path.to_path_buf(),
        })
    }
}

/// A range of the ring, ready to upload.
#[derive(Debug, Clone)]
pub struct Clip {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub init: Vec<u8>,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub segments: Vec<ClipSegment>,
}

#[derive(Debug, Clone)]
pub struct ClipSegment {
    pub start: DateTime<Utc>,
    pub duration_ms: u64,
    pub bytes: Vec<u8>,
}

/// What an init segment describes, beside the init itself, because nothing
/// here reads an avc1 box back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct InitDescription {
    codec: String,
    width: u32,
    height: u32,
}

/// One stretch of recording that shares an init segment.
#[derive(Default)]
struct Run {
    origin: Option<(i64, DateTime<Utc>)>,
    init: Option<String>,
}

/// One camera's ring buffer.
pub struct Ring {
    dir: PathBuf,
    budget_bytes: u64,
}

impl Ring {
    /// The ring for one camera under `root`, with the disk it may use.
    pub fn open(root: &Path, camera_id: &str, budget_bytes: u64) -> anyhow::Result<Self> {
        // A camera id is a uuid the gateway derived, but it arrives from the
        // control plane, so it does not get to name a directory outside this
        // one.
        if camera_id.is_empty()
            || !camera_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(anyhow!("a camera id may not name a directory: {camera_id}"));
        }
        let dir = root.join(camera_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create the ring directory {}", dir.display()))?;
        Ok(Self { dir, budget_bytes })
    }

    /// Every segment on disk, oldest first.
    pub fn segments(&self) -> anyhow::Result<Vec<StoredSegment>> {
        let mut segments = Vec::new();
        for entry in std::fs::read_dir(&self.dir)
            .with_context(|| format!("read the ring directory {}", self.dir.display()))?
        {
            let path = entry?.path();
            if let Some(segment) = StoredSegment::parse(&path) {
                segments.push(segment);
            }
        }
        segments.sort_by_key(|segment| (segment.start, segment.duration_ms));
        Ok(segments)
    }

    /// How far back the ring goes, and how far forward.
    pub fn span(&self) -> anyhow::Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        let segments = self.segments()?;
        match (segments.first(), segments.last()) {
            (Some(first), Some(last)) => Ok(Some((first.start, last.end()))),
            _ => Ok(None),
        }
    }

    pub fn init_bytes(&self, init: &str) -> anyhow::Result<Vec<u8>> {
        let path = self.dir.join(format!("init-{init}.mp4"));
        std::fs::read(&path).with_context(|| format!("read {}", path.display()))
    }

    /// Record until the source ends or `stop` is triggered.
    pub async fn record(
        &self,
        source: &mut dyn FrameSource,
        stop: &tokio::sync::Notify,
    ) -> anyhow::Result<()> {
        let mut segmenter = Segmenter::new(SEGMENT_DURATION)?;
        let mut run = Run::default();

        loop {
            let frame = tokio::select! {
                _ = stop.notified() => break,
                frame = source.next_frame() => frame?,
            };
            let Some(frame) = frame else { break };
            let parameters = source.parameters();

            match segmenter.push(&frame, parameters.as_ref())? {
                Pushed::Nothing => {}
                Pushed::Segment(segment) => {
                    self.store(&mut segmenter, segment, &mut run)?;
                    self.prune()?;
                }
                Pushed::ParametersChanged => {
                    // The stream changed shape: what came before decodes with
                    // one init and what comes after with another, so this
                    // segmenter is finished — but what it holds is video, and
                    // dropping it would lose the seconds around the change.
                    if let Some(segment) = segmenter.flush()? {
                        self.store(&mut segmenter, segment, &mut run)?;
                    }
                    segmenter = Segmenter::new(SEGMENT_DURATION)?;
                    // The stream did not stop, only changed shape, and its
                    // clock kept running: keeping the origin keeps the two
                    // halves in one timeline instead of stacking them on top
                    // of each other. Only the init has to change.
                    run.init = None;
                    match segmenter.push(&frame, parameters.as_ref())? {
                        Pushed::ParametersChanged => {
                            return Err(anyhow!("a frame changed parameters against itself"));
                        }
                        Pushed::Segment(segment) => {
                            self.store(&mut segmenter, segment, &mut run)?;
                        }
                        Pushed::Nothing => {}
                    }
                    self.prune()?;
                }
            }
        }

        if let Some(segment) = segmenter.flush()? {
            self.store(&mut segmenter, segment, &mut run)?;
            self.prune()?;
        }
        Ok(())
    }

    /// Put one segment on disk, writing the init it needs the first time.
    ///
    /// The first segment of a run fixes the clock: it has just ended, so it
    /// started one duration ago. Everything after it is placed by the
    /// stream's own timestamps, so a camera whose clock drifts drifts with it
    /// rather than jumping.
    fn store(
        &self,
        segmenter: &mut Segmenter,
        segment: crate::segmenter::ClosedSegment,
        run: &mut Run,
    ) -> anyhow::Result<StoredSegment> {
        let clock_rate = segmenter
            .clock_rate()
            .ok_or_else(|| anyhow!("missing RTP clock rate"))?;
        let duration_ms = ticks_to_ms(segment.duration_ticks, clock_rate);
        let (first_timestamp, wall_clock) = *run.origin.get_or_insert((
            segment.start_timestamp,
            Utc::now() - chrono::Duration::milliseconds(duration_ms as i64),
        ));
        let init = match &run.init {
            Some(init) => init.clone(),
            None => {
                let description = InitDescription {
                    codec: segmenter
                        .codec()
                        .ok_or_else(|| anyhow!("missing H264 codec parameters"))?
                        .to_owned(),
                    width: segmenter
                        .dimensions()
                        .ok_or_else(|| anyhow!("missing video dimensions"))?
                        .0,
                    height: segmenter
                        .dimensions()
                        .ok_or_else(|| anyhow!("missing video dimensions"))?
                        .1,
                };
                let bytes = segmenter.init()?;
                let init = self.write_init(&bytes, &description)?;
                run.init = Some(init.clone());
                init
            }
        };

        let offset_ms = ticks_to_ms(
            segment
                .start_timestamp
                .saturating_sub(first_timestamp)
                .max(0) as u64,
            clock_rate,
        );
        let stored = StoredSegment {
            start: wall_clock + chrono::Duration::milliseconds(offset_ms as i64),
            duration_ms,
            init,
            path: PathBuf::new(),
        };
        let path = self.dir.join(stored.file_name());
        write_atomically(&path, &segment.bytes)?;
        Ok(StoredSegment { path, ..stored })
    }

    /// Write the init segment under a name derived from its own bytes, so a
    /// stream that comes back the same shape reuses it, with a note of what it
    /// describes beside it.
    fn write_init(&self, bytes: &[u8], description: &InitDescription) -> anyhow::Result<String> {
        let name = short_hash(bytes);
        let path = self.dir.join(format!("init-{name}.mp4"));
        if !path.exists() {
            write_atomically(&path, bytes)?;
        }
        let description_path = self.dir.join(format!("init-{name}.json"));
        if !description_path.exists() {
            write_atomically(&description_path, &serde_json::to_vec(description)?)?;
        }
        Ok(name)
    }

    /// Everything needed to upload a clip: one init and the media segments
    /// that cover the range, in order.
    ///
    /// Segments start on keyframes, so a range that begins inside one widens
    /// backwards to that segment's start: a clip that began mid-GOP would not
    /// decode.
    pub fn clip(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Clip> {
        if to <= from {
            return Err(anyhow!("a clip needs a range, not a moment"));
        }
        let segments = self.segments()?;
        let (Some(oldest), Some(newest)) = (segments.first(), segments.last()) else {
            return Err(anyhow!("this camera has nothing recorded"));
        };
        let (held_from, held_to) = (oldest.start, newest.end());
        let wanted: Vec<StoredSegment> = segments
            .iter()
            .filter(|segment| segment.end() > from && segment.start < to)
            .cloned()
            .collect();
        let (Some(first), Some(last)) = (wanted.first(), wanted.last()) else {
            // Asking for a window the ring never held or has already dropped.
            // Saying what it does hold is the difference between a usable
            // error and a shrug.
            return Err(anyhow!(
                "nothing recorded between those times; the ring holds {} to {}, which is {} seconds of video",
                held_from.to_rfc3339(),
                held_to.to_rfc3339(),
                (held_to - held_from).num_seconds()
            ));
        };
        // Asking for more than the ring kept is normal — "save the last hour"
        // on a ring holding twenty minutes. Hand over what exists; the clip
        // says what range it actually covers.
        if from < held_from {
            tracing::info!(
                asked_from = %from.to_rfc3339(),
                held_from = %held_from.to_rfc3339(),
                "a clip was asked to reach further back than the ring goes"
            );
        }
        // A clip is one track. Two inits means the camera changed shape in the
        // middle, and no player would sit through that as one file.
        if wanted.iter().any(|segment| segment.init != first.init) {
            return Err(anyhow!(
                "the camera changed resolution during that range; ask for a range either side of it"
            ));
        }
        let (start, end) = (first.start, last.end());
        let init = first.init.clone();
        let mut media = Vec::with_capacity(wanted.len());
        for segment in wanted {
            media.push(ClipSegment {
                start: segment.start,
                duration_ms: segment.duration_ms,
                bytes: std::fs::read(&segment.path)
                    .with_context(|| format!("read {}", segment.path.display()))?,
            });
        }
        let description = self.init_description(&init)?;
        Ok(Clip {
            start,
            end,
            init: self.init_bytes(&init)?,
            codec: description.codec,
            width: description.width,
            height: description.height,
            segments: media,
        })
    }

    fn init_description(&self, init: &str) -> anyhow::Result<InitDescription> {
        let path = self.dir.join(format!("init-{init}.json"));
        let raw = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Delete oldest-first until the ring is inside its budget. Returns how
    /// many bytes went.
    pub fn prune(&self) -> anyhow::Result<u64> {
        let segments = self.segments()?;
        let mut sizes = Vec::with_capacity(segments.len());
        let mut total = 0_u64;
        for segment in &segments {
            let size = std::fs::metadata(&segment.path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            total += size;
            sizes.push(size);
        }
        let mut freed = 0_u64;
        let mut index = 0;
        // The newest segment stays whatever the budget says: a ring that
        // deletes what it just wrote is not a ring, it is a shredder.
        while total > self.budget_bytes && index + 1 < segments.len() {
            if std::fs::remove_file(&segments[index].path).is_ok() {
                total -= sizes[index];
                freed += sizes[index];
            }
            index += 1;
        }
        self.prune_unused_inits()?;
        Ok(freed)
    }

    /// An init segment no remaining media segment names is dead weight.
    fn prune_unused_inits(&self) -> anyhow::Result<()> {
        let live: std::collections::HashSet<String> = self
            .segments()?
            .into_iter()
            .map(|segment| segment.init)
            .collect();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(hash) = name
                .strip_prefix("init-")
                .and_then(|n| n.strip_suffix(".mp4"))
            else {
                continue;
            };
            if !live.contains(hash) {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(self.dir.join(format!("init-{hash}.json")));
            }
        }
        Ok(())
    }
}

/// Write to a temporary name and rename into place, so a reader never sees a
/// half-written segment and a crash never leaves one.
fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension("partial");
    std::fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Enough of a digest to tell two inits apart, short enough to read. (This
/// module is not called `ring` because that name belongs to the crate doing
/// the hashing.)
fn short_hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref()[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{Frame, FrameSource, VideoParameters, testing::ScriptedSource};
    use bytes::Bytes;

    const AVCC: [u8; 19] = [
        1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1, 0, 4, 0x68, 0xee, 0x3c,
        0x80,
    ];

    fn parameters(width: u32) -> VideoParameters {
        VideoParameters {
            rfc6381_codec: "avc1.42e01e".into(),
            pixel_dimensions: (width, 480),
            extra_data: AVCC.to_vec(),
            frame_rate: Some((1, 25)),
        }
    }

    /// 25 fps at 90 kHz, a keyframe every second, each frame a kilobyte so the
    /// budget tests deal in numbers a human can check.
    fn frames(count: i64) -> std::collections::VecDeque<Frame> {
        (0..count)
            .map(|index| Frame {
                data: Bytes::from(vec![0_u8; 1024]),
                timestamp: index * 3_600,
                clock_rate: 90_000,
                keyframe: index % 25 == 0,
                new_parameters: index == 0,
            })
            .collect()
    }

    fn scripted(count: i64) -> ScriptedSource {
        ScriptedSource {
            frames: frames(count),
            parameters: Some(parameters(640)),
        }
    }

    #[tokio::test]
    async fn segments_land_on_disk_with_their_own_range_in_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        ring.record(&mut scripted(250), &stop).await.unwrap();

        let segments = ring.segments().unwrap();
        assert!(segments.len() >= 2, "ten seconds in four-second segments");
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[0].start <= pair[1].start)
        );
        // Names are the index: a fresh Ring over the same directory sees the
        // same segments without being told anything.
        let reopened = Ring::open(dir.path(), "camera-1", 1).unwrap();
        assert_eq!(reopened.segments().unwrap(), segments);
        let (from, to) = ring.span().unwrap().expect("a span");
        assert!(to > from);
        assert!(!ring.init_bytes(&segments[0].init).unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_budget_throws_away_the_oldest_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        // Each four-second segment is about 100 frames of a kilobyte, so this
        // holds two of them and change.
        let ring = Ring::open(dir.path(), "camera-1", 250 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        ring.record(&mut scripted(750), &stop).await.unwrap();

        let segments = ring.segments().unwrap();
        let total: u64 = segments
            .iter()
            .map(|segment| std::fs::metadata(&segment.path).unwrap().len())
            .sum();
        assert!(total <= 250 * 1024, "{total} bytes is over budget");
        assert!(!segments.is_empty(), "a ring that keeps nothing is useless");
        // What survives is the end of the stream, not the start of it.
        let (from, _) = ring.span().unwrap().unwrap();
        assert!(from > Utc::now() - chrono::Duration::seconds(60));
    }

    #[tokio::test]
    async fn an_init_segment_goes_when_its_last_segment_does() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 4 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        ring.record(&mut scripted(250), &stop).await.unwrap();
        let init = ring.segments().unwrap()[0].init.clone();
        assert!(ring.init_bytes(&init).is_ok());

        for segment in ring.segments().unwrap() {
            std::fs::remove_file(&segment.path).unwrap();
        }
        ring.prune().unwrap();
        assert!(
            ring.init_bytes(&init).is_err(),
            "an init nothing decodes with is dead weight"
        );
    }

    /// A camera that comes back at a different resolution part-way through a
    /// single session: the frames after it cannot be decoded with the init
    /// written for the frames before it.
    struct ReshapingSource {
        frames: std::collections::VecDeque<Frame>,
        current: VideoParameters,
        after: VideoParameters,
        switch_at: i64,
    }

    #[async_trait::async_trait]
    impl FrameSource for ReshapingSource {
        async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
            let frame = self.frames.pop_front();
            if let Some(frame) = frame
                .as_ref()
                .filter(|frame| frame.new_parameters && frame.timestamp >= self.switch_at)
            {
                let _ = frame;
                self.current = self.after.clone();
            }
            Ok(frame)
        }

        fn parameters(&self) -> Option<VideoParameters> {
            Some(self.current.clone())
        }
    }

    #[tokio::test]
    async fn a_stream_that_changes_shape_gets_a_second_init() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();

        let switch_at = 75 * 3_600;
        let mut source = ReshapingSource {
            frames: frames(150)
                .into_iter()
                .map(|mut frame| {
                    // The camera announces new parameter sets where it changed.
                    frame.new_parameters = frame.timestamp == 0 || frame.timestamp == switch_at;
                    frame.keyframe = frame.keyframe || frame.timestamp == switch_at;
                    frame
                })
                .collect(),
            current: parameters(640),
            after: parameters(1280),
            switch_at,
        };
        ring.record(&mut source, &stop).await.unwrap();

        let segments = ring.segments().unwrap();
        let inits: std::collections::HashSet<String> = segments
            .iter()
            .map(|segment| segment.init.clone())
            .collect();
        assert_eq!(inits.len(), 2, "two shapes, two init segments");
        for init in &inits {
            assert!(
                !ring.init_bytes(init).unwrap().is_empty(),
                "both inits are on disk"
            );
            assert!(
                segments.iter().any(|segment| &segment.init == init),
                "an init nothing uses should not have been written"
            );
        }
        // The seconds either side of the change are video like any other. The
        // first version of this dropped whatever the old segmenter still held
        // when nothing had closed a segment yet, which on a three-second
        // stream was all of it.
        let covered: u64 = segments.iter().map(|segment| segment.duration_ms).sum();
        assert!(
            covered >= 5_000,
            "only {covered} ms of six seconds survived"
        );
    }

    #[tokio::test]
    async fn a_clip_comes_out_of_the_ring_with_the_init_that_decodes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        ring.record(&mut scripted(500), &stop).await.unwrap();

        let (from, to) = ring.span().unwrap().unwrap();
        // Ask for a slice out of the middle, starting inside a segment.
        let asked_from = from + chrono::Duration::milliseconds(5_500);
        let clip = ring.clip(asked_from, to).unwrap();
        assert!(!clip.init.is_empty());
        assert_eq!(clip.codec, "avc1.42e01e");
        assert_eq!((clip.width, clip.height), (640, 480));
        assert!(!clip.segments.is_empty());
        assert!(
            clip.start <= asked_from,
            "a clip widens back to a keyframe, not forward from one"
        );
        assert!(
            clip.segments
                .iter()
                .all(|segment| !segment.bytes.is_empty())
        );
        assert!(
            clip.segments
                .windows(2)
                .all(|pair| pair[0].start < pair[1].start),
            "segments are in order"
        );
    }

    #[tokio::test]
    async fn a_range_the_ring_has_already_dropped_says_how_far_back_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        ring.record(&mut scripted(250), &stop).await.unwrap();
        let (from, to) = ring.span().unwrap().unwrap();

        let error = ring
            .clip(
                from - chrono::Duration::hours(2),
                from - chrono::Duration::hours(1),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("nothing recorded between those times"),
            "{error}"
        );
        assert!(
            error.contains(&from.to_rfc3339()),
            "it says what the ring does hold: {error}"
        );

        // Asking for more than the ring kept is not an error: an operator
        // pressing "save the last hour" gets the hour the ring has.
        let clip = ring.clip(from - chrono::Duration::hours(1), to).unwrap();
        assert!(clip.start >= from, "it cannot invent video it never had");
        assert!(clip.end <= to);
        assert!(!clip.segments.is_empty());
    }

    #[tokio::test]
    async fn a_clip_may_not_span_a_change_of_shape() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = tokio::sync::Notify::new();
        let switch_at = 75 * 3_600;
        let mut source = ReshapingSource {
            frames: frames(300)
                .into_iter()
                .map(|mut frame| {
                    frame.new_parameters = frame.timestamp == 0 || frame.timestamp == switch_at;
                    frame.keyframe = frame.keyframe || frame.timestamp == switch_at;
                    frame
                })
                .collect(),
            current: parameters(640),
            after: parameters(1280),
            switch_at,
        };
        ring.record(&mut source, &stop).await.unwrap();

        let (from, to) = ring.span().unwrap().unwrap();
        let error = ring.clip(from, to).unwrap_err().to_string();
        assert!(error.contains("changed resolution"), "{error}");
        // Either side of the change is still perfectly askable.
        let segments = ring.segments().unwrap();
        let first_init = &segments[0].init;
        let last_of_first = segments
            .iter()
            .take_while(|segment| &segment.init == first_init)
            .last()
            .unwrap();
        ring.clip(from, last_of_first.end())
            .expect("the first shape alone");
    }

    #[tokio::test]
    async fn an_empty_ring_refuses_a_clip_rather_than_returning_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 1024).unwrap();
        let now = Utc::now();
        assert!(ring.clip(now - chrono::Duration::minutes(1), now).is_err());
        assert!(ring.clip(now, now).is_err(), "a moment is not a range");
    }

    #[tokio::test]
    async fn a_camera_id_may_not_escape_the_ring_directory() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["../etc", "a/b", "", "."] {
            assert!(
                Ring::open(dir.path(), id, 1024).is_err(),
                "{id} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn recording_stops_when_it_is_told_to() {
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::open(dir.path(), "camera-1", 10 * 1024 * 1024).unwrap();
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let waiting = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waiting.notify_waiters();
        });
        // A source that never ends: only the stop can end this.
        let mut endless = crate::frames::testing::EndlessSource::new(parameters(640));
        tokio::time::timeout(Duration::from_secs(5), ring.record(&mut endless, &stop))
            .await
            .expect("record must return when stopped")
            .unwrap();
    }
}
