//! Video pushed to the gateway, rather than pulled from a camera.
//!
//! An encoder, an NVR or another VMS publishes to `rtmp://<gateway>/live/<key>`
//! and the frames come out of a [`crate::frames::FrameSource`] like any other.
//! Only keys the control plane listed as sources are accepted: a stranger who
//! finds the port cannot publish into someone's recordings.
//!
//! The video never leaves the site. The listener is on the gateway, so a
//! pushed stream costs the same as a camera — which is the whole reason the
//! design puts it here and not in the cloud.
//! See docs/superpowers/specs/2026-09-22-video-sources-design.md.

pub mod flv;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Context;
use bytes::Bytes;
use rml_rtmp::{
    handshake::{Handshake, HandshakeProcessResult, PeerType},
    sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::{
    frames::Frame,
    ingest::{Ingest, StreamState, lock},
};

/// RTMP counts in milliseconds, and says so nowhere: it is simply the unit of
/// every timestamp in the protocol.
const RTMP_CLOCK_RATE: u32 = 1_000;

/// Accept publishers until the process ends.
pub async fn serve(listener: TcpListener, ingest: Ingest) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "RTMP accept failed");
                continue;
            }
        };
        let ingest = ingest.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(socket, ingest).await {
                tracing::info!(%peer, %error, "RTMP connection ended");
            }
        });
    }
}

async fn handle_connection(mut socket: TcpStream, ingest: Ingest) -> anyhow::Result<()> {
    let _ = socket.set_nodelay(true);
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut handshake = Handshake::new(PeerType::Server);
    let leftover = loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        match handshake
            .process_bytes(&buffer[..read])
            .map_err(|error| anyhow::anyhow!("RTMP handshake failed: {error:?}"))?
        {
            HandshakeProcessResult::InProgress { response_bytes } => {
                socket.write_all(&response_bytes).await?;
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                socket.write_all(&response_bytes).await?;
                break remaining_bytes;
            }
        }
    };

    let (mut session, results) = ServerSession::new(ServerSessionConfig::new())
        .map_err(|error| anyhow::anyhow!("RTMP session failed to start: {error:?}"))?;
    let mut connection = Connection {
        ingest,
        publishing: None,
        state: None,
    };
    connection.apply(&mut session, results, &mut socket).await?;

    let results = session
        .handle_input(&leftover)
        .map_err(|error| anyhow::anyhow!("RTMP input rejected: {error:?}"))?;
    connection.apply(&mut session, results, &mut socket).await?;

    loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let results = session
            .handle_input(&buffer[..read])
            .map_err(|error| anyhow::anyhow!("RTMP input rejected: {error:?}"))?;
        connection.apply(&mut session, results, &mut socket).await?;
    }
    Ok(())
}

/// The part of a connection that outlives one batch of results.
struct Connection {
    ingest: Ingest,
    publishing: Option<String>,
    state: Option<Arc<Mutex<StreamState>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(key) = self.publishing.take() {
            self.ingest.close(&key);
        }
    }
}

impl Connection {
    async fn apply(
        &mut self,
        session: &mut ServerSession,
        results: Vec<ServerSessionResult>,
        socket: &mut TcpStream,
    ) -> anyhow::Result<()> {
        let mut queue = results;
        while let Some(result) = queue.pop_first_in_order() {
            match result {
                ServerSessionResult::OutboundResponse(packet) => {
                    socket.write_all(&packet.bytes).await?;
                }
                ServerSessionResult::RaisedEvent(event) => {
                    let more = self.on_event(session, event)?;
                    queue.extend(more);
                }
                ServerSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(())
    }

    fn on_event(
        &mut self,
        session: &mut ServerSession,
        event: ServerSessionEvent,
    ) -> anyhow::Result<Vec<ServerSessionResult>> {
        match event {
            ServerSessionEvent::ConnectionRequested { request_id, .. } => session
                .accept_request(request_id)
                .map_err(|error| anyhow::anyhow!("RTMP connection not accepted: {error:?}")),
            ServerSessionEvent::PublishStreamRequested {
                request_id,
                stream_key,
                ..
            } => {
                if !self.ingest.is_allowed(&stream_key) {
                    tracing::warn!(key = %stream_key, "refused an RTMP publisher: no such source");
                    return session
                        .reject_request(
                            request_id,
                            "NetStream.Publish.Denied",
                            "no video source is registered for this stream key",
                        )
                        .map_err(|error| anyhow::anyhow!("RTMP rejection failed: {error:?}"));
                }
                self.state = Some(self.ingest.open(&stream_key));
                self.publishing = Some(stream_key.clone());
                tracing::info!(key = %stream_key, "an RTMP publisher started");
                session
                    .accept_request(request_id)
                    .map_err(|error| anyhow::anyhow!("RTMP publish not accepted: {error:?}"))
            }
            ServerSessionEvent::PublishStreamFinished { stream_key, .. } => {
                self.ingest.close(&stream_key);
                if self.publishing.as_deref() == Some(stream_key.as_str()) {
                    self.publishing = None;
                }
                Ok(Vec::new())
            }
            ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
                if let Some(state) = &self.state {
                    let mut state = lock(state);
                    if let (Some(width), Some(height)) =
                        (metadata.video_width, metadata.video_height)
                    {
                        state.dimensions = Some((width, height));
                    }
                    // The archive wants how long a frame lasts, in seconds.
                    state.frame_rate = metadata
                        .video_frame_rate
                        .filter(|rate| *rate > 0.0)
                        .map(|rate| (1_000, (rate * 1_000.0).round() as u32));
                }
                Ok(Vec::new())
            }
            ServerSessionEvent::VideoDataReceived {
                stream_key,
                data,
                timestamp,
                ..
            } => {
                self.on_video(&stream_key, data, i64::from(timestamp.value))?;
                Ok(Vec::new())
            }
            // Audio is out of scope, and everything else is bookkeeping the
            // session handled for us.
            _ => Ok(Vec::new()),
        }
    }

    fn on_video(&mut self, key: &str, data: Bytes, timestamp: i64) -> anyhow::Result<()> {
        let Some(state) = self.state.clone() else {
            return Ok(());
        };
        match flv::parse_video_tag(&data).context("FLV video tag")? {
            flv::VideoTag::Parameters(avcc) => {
                let codec = flv::rfc6381_codec(&avcc)?;
                let mut state = lock(&state);
                let changed = state.avcc.as_deref() != Some(avcc.as_slice());
                state.codec = Some(codec);
                state.avcc = Some(avcc);
                if changed {
                    tracing::info!(key = %key, "an RTMP publisher sent decoder configuration");
                }
            }
            flv::VideoTag::Frame {
                data: payload,
                keyframe,
            } => {
                let new_parameters = {
                    let mut state = lock(&state);
                    state.frames += 1;
                    state.bytes += payload.len() as u64;
                    state.last_frame = Some(Instant::now());
                    state.frames == 1
                };
                self.ingest.publish(
                    key,
                    Frame {
                        data: payload,
                        timestamp,
                        clock_rate: RTMP_CLOCK_RATE,
                        keyframe,
                        new_parameters,
                    },
                );
            }
            flv::VideoTag::End => {}
        }
        Ok(())
    }
}

/// Results must be sent in the order they came out of the session, so this is
/// a queue, not a stack.
trait ResultQueue {
    fn pop_first_in_order(&mut self) -> Option<ServerSessionResult>;
}

impl ResultQueue for Vec<ServerSessionResult> {
    fn pop_first_in_order(&mut self) -> Option<ServerSessionResult> {
        if self.is_empty() {
            None
        } else {
            Some(self.remove(0))
        }
    }
}

/// Where the listener binds, if push ingest is on at all.
pub fn listen_address() -> Option<SocketAddr> {
    let raw = std::env::var("RTMP_LISTEN").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse() {
        Ok(address) => Some(address),
        Err(error) => {
            tracing::warn!(%error, value = %raw, "RTMP_LISTEN is not an address; push ingest is off");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_publisher::FakePublisher;
    use crate::frames::FrameSource as _;
    use std::time::Duration;

    /// A sequence header and then frames, exactly as an encoder writes them.
    fn sequence_header() -> Bytes {
        Bytes::from_static(&[
            0x17, 0x00, 0, 0, 0, 1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1,
            0, 4, 0x68, 0xee, 0x3c, 0x80,
        ])
    }

    fn video_tag(keyframe: bool, payload: u8) -> Bytes {
        let mut tag = vec![if keyframe { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
        tag.extend_from_slice(&[0, 0, 0, 2, 0x65, payload]);
        Bytes::from(tag)
    }

    async fn listening(ingest: &Ingest) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let ingest = ingest.clone();
        tokio::spawn(async move { serve(listener, ingest).await });
        address
    }

    #[tokio::test]
    async fn a_publisher_becomes_a_frame_source() {
        let ingest = Ingest::new();
        ingest.allow(["front-door".to_string()]);
        let address = listening(&ingest).await;

        let mut publisher = FakePublisher::publish_to(address, "front-door")
            .await
            .expect("the gateway accepts a listed stream key");
        publisher.send_metadata(1280, 720, 25.0).await.unwrap();
        publisher.send_video(sequence_header(), 0).await.unwrap();

        // Subscribing needs the stream to exist, which the publish request made.
        let mut source = loop {
            if let Some(source) = ingest.subscribe("front-door") {
                break source;
            }
            tokio::task::yield_now().await;
        };

        publisher.send_video(video_tag(true, 1), 0).await.unwrap();
        publisher.send_video(video_tag(false, 2), 40).await.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
            .await
            .expect("a frame arrives")
            .unwrap()
            .expect("not the end of the stream");
        assert!(first.keyframe);
        assert_eq!(first.clock_rate, RTMP_CLOCK_RATE);
        assert_eq!(first.timestamp, 0);
        assert_eq!(&first.data[..], &[0, 0, 0, 2, 0x65, 1]);

        let second = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!second.keyframe);
        assert_eq!(second.timestamp, 40, "RTMP timestamps are milliseconds");

        let parameters = source.parameters().expect("the publisher described itself");
        assert_eq!(parameters.rfc6381_codec, "avc1.42e01e");
        assert_eq!(parameters.pixel_dimensions, (1280, 720));
        assert_eq!(parameters.extra_data[0], 1);

        let stats = ingest.stats("front-door", Duration::from_secs(5)).unwrap();
        assert_eq!(stats.frames, 2);
        assert!(stats.publishing);
    }

    /// Everything above proves this gateway understands `rml_rtmp`. This
    /// proves it understands ffmpeg, which is what an encoder actually is.
    ///
    /// Run with `make check-ingest`. It publishes the committed H.264 fixture
    /// with `-c copy`, so the bytes are the fixture's and the framing,
    /// timing and handshake are ffmpeg's.
    #[tokio::test]
    #[ignore = "needs ffmpeg on the machine; run make check-ingest"]
    async fn ffmpeg_can_publish_to_this_gateway() {
        let ingest = Ingest::new();
        ingest.allow(["yard".to_string()]);
        let address = listening(&ingest).await;

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/camera.h264");
        // std rather than tokio: spawning returns at once, and the only
        // other thing this needs is to kill it, which does not block either.
        let mut encoder = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                // Real time, as a camera feeds an encoder.
                "-re",
                "-f",
                "h264",
                "-i",
                fixture,
                "-c",
                "copy",
                "-f",
                "flv",
                &format!("rtmp://{address}/live/yard"),
            ])
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("ffmpeg must be on PATH; run make check-ingest");

        let mut source = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(source) = ingest.subscribe("yard") {
                    return source;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("ffmpeg never got as far as publishing");

        let mut frames = Vec::new();
        while frames.len() < 25 {
            let frame = tokio::time::timeout(Duration::from_secs(20), source.next_frame())
                .await
                .expect("ffmpeg stopped sending")
                .expect("the stream failed");
            match frame {
                Some(frame) => frames.push(frame),
                None => break,
            }
        }
        // Kill it and reap it: a zombie ffmpeg per run would outlive the
        // test binary.
        let _ = encoder.kill();
        let _ = encoder.wait();

        assert!(frames.len() >= 2, "got {} frames", frames.len());
        assert!(
            frames.iter().any(|frame| frame.keyframe),
            "no keyframe arrived, so nothing could be recorded"
        );
        assert!(
            frames
                .iter()
                .all(|frame| frame.clock_rate == RTMP_CLOCK_RATE),
            "RTMP timestamps are milliseconds"
        );
        assert!(
            frames
                .windows(2)
                .all(|pair| pair[1].timestamp >= pair[0].timestamp),
            "timestamps moved backwards"
        );
        let parameters = source
            .parameters()
            .expect("ffmpeg describes the stream in its sequence header");
        assert_eq!(parameters.pixel_dimensions, (320, 240));
        assert!(parameters.rfc6381_codec.starts_with("avc1."));
        assert_eq!(parameters.extra_data[0], 1, "an AVCC record");
    }

    #[tokio::test]
    async fn a_stream_key_nobody_registered_is_refused() {
        // The port may be reachable from the camera network. Only keys the
        // control plane listed as sources may publish.
        let ingest = Ingest::new();
        ingest.allow(["front-door".to_string()]);
        let address = listening(&ingest).await;

        let error = match FakePublisher::publish_to(address, "not-a-source").await {
            // The refusal is the cause, not the outer context.
            Err(error) => format!("{error:#}"),
            Ok(_) => panic!("an unlisted key must be refused"),
        };
        assert!(
            error.contains("Denied") || error.contains("refused"),
            "{error}"
        );
        assert!(ingest.subscribe("not-a-source").is_none());
    }

    #[tokio::test]
    async fn a_pushed_stream_records_like_a_camera() {
        // The point of the frame source: the recorder does not know this video
        // was pushed rather than pulled.
        let ingest = Ingest::new();
        ingest.allow(["loading-bay".to_string()]);
        let address = listening(&ingest).await;

        let mut publisher = FakePublisher::publish_to(address, "loading-bay")
            .await
            .unwrap();
        publisher.send_metadata(640, 480, 25.0).await.unwrap();
        publisher.send_video(sequence_header(), 0).await.unwrap();
        let mut source = loop {
            if let Some(source) = ingest.subscribe("loading-bay") {
                break source;
            }
            tokio::task::yield_now().await;
        };

        let publishing = tokio::spawn(async move {
            for index in 0..8_u32 {
                publisher
                    .send_video(video_tag(index % 4 == 0, index as u8), index * 40)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            publisher.stop().await.unwrap();
        });

        let recording = crate::archive::record_from(
            &mut source,
            Duration::from_millis(160),
            Duration::from_millis(160),
        )
        .await
        .expect("a pushed stream records");
        publishing.await.unwrap();

        assert_eq!(recording.codec, "avc1.42e01e");
        assert_eq!((recording.width, recording.height), (640, 480));
        assert!(!recording.segments.is_empty());
    }

    #[tokio::test]
    async fn a_publisher_that_leaves_ends_the_stream() {
        let ingest = Ingest::new();
        ingest.allow(["gate".to_string()]);
        let address = listening(&ingest).await;

        let mut publisher = FakePublisher::publish_to(address, "gate").await.unwrap();
        publisher.send_video(sequence_header(), 0).await.unwrap();
        let mut source = loop {
            if let Some(source) = ingest.subscribe("gate") {
                break source;
            }
            tokio::task::yield_now().await;
        };
        publisher.send_video(video_tag(true, 1), 0).await.unwrap();
        drop(publisher);

        let mut frames = 0;
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
                .await
                .expect("the source must not hang once the publisher is gone")
                .unwrap();
            match frame {
                Some(_) => frames += 1,
                // Ending is the point: live view and the recorder both stop on it.
                None => break,
            }
        }
        assert_eq!(frames, 1);
        assert!(ingest.subscribe("gate").is_none(), "the stream is gone");
    }
}
