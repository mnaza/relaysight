//! Where encoded video comes from, whatever speaks it.
//!
//! Every media path in this gateway — live view, recording, the probe — used to
//! open its own RTSP session, so a stream that arrives any other way had nowhere
//! to go. A source is now a thing that yields H.264 access units, and RTSP is
//! one implementation of it.
//!
//! Framing is chosen when the source is opened, not converted afterwards: a
//! live session wants Annex B with parameter sets on every key frame, and the
//! recorder wants four-byte lengths with the parameters out of band. Asking for
//! what each consumer already wanted keeps their bytes exactly as they were.
//! The probe in `rtsp.rs` stays on retina deliberately: it counts lost RTP
//! packets and reports whatever codec the SDP named, including ones this
//! gateway will not carry. Neither survives an abstraction over access units.
//! See docs/superpowers/specs/2026-09-22-video-sources-design.md.

use anyhow::{Context, anyhow};
use bytes::Bytes;
use retina::client::{PlayOptions, SessionOptions, SetupOptions};
use retina::codec::{CodecItem, FrameFormat, ParametersRef};
use std::time::Duration;

/// How a consumer wants NAL units framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Annex B start codes, parameter sets on every key frame: what a decoder
    /// or a WebRTC sample writer takes as it comes.
    AnnexB,
    /// Four-byte lengths, parameter sets out of band: what fMP4 stores.
    Avcc,
}

impl From<Framing> for FrameFormat {
    fn from(framing: Framing) -> Self {
        match framing {
            Framing::AnnexB => FrameFormat::SIMPLE,
            Framing::Avcc => FrameFormat::MP4,
        }
    }
}

/// One encoded access unit, as it came off the wire.
#[derive(Clone, Debug)]
pub struct Frame {
    pub data: Bytes,
    /// In `clock_rate` units, as the source counts them.
    pub timestamp: i64,
    pub clock_rate: u32,
    pub keyframe: bool,
    /// The stream's parameters changed with this frame.
    pub new_parameters: bool,
}

/// What a recorder needs to describe the stream it is storing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoParameters {
    pub rfc6381_codec: String,
    pub pixel_dimensions: (u32, u32),
    /// An AVCDecoderConfigurationRecord.
    pub extra_data: Vec<u8>,
    /// Numerator and denominator, when the stream declares one. The recorder
    /// falls back to it for the duration of a frame it cannot measure.
    pub frame_rate: Option<(u32, u32)>,
}

#[async_trait::async_trait]
pub trait FrameSource: Send {
    /// The next access unit, or `None` when the stream ended.
    async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>>;

    /// The stream's out-of-band parameters, once they are known.
    fn parameters(&self) -> Option<VideoParameters>;
}

/// An RTSP session, which is where video has come from until now.
pub struct RtspSource {
    demuxed: std::pin::Pin<Box<retina::client::Demuxed>>,
    video_stream: usize,
}

impl RtspSource {
    /// DESCRIBE, SETUP and PLAY, refusing anything this gateway cannot carry
    /// without transcoding.
    pub async fn open(
        raw_url: &str,
        username: Option<&str>,
        password: Option<&str>,
        framing: Framing,
    ) -> anyhow::Result<Self> {
        let (url, creds) = crate::rtsp::split_credentials(raw_url, username, password)?;
        let session = tokio::time::timeout(
            Duration::from_secs(8),
            retina::client::Session::describe(
                url,
                SessionOptions::default()
                    .creds(creds)
                    .user_agent(format!("vms-gateway/{}", crate::VERSION)),
            ),
        )
        .await
        .context("RTSP DESCRIBE timeout")??;
        let mut session = session;
        let video_stream = session
            .streams()
            .iter()
            .position(|stream| stream.media().eq_ignore_ascii_case("video"))
            .ok_or_else(|| anyhow!("RTSP source has no video stream"))?;
        let encoding = session.streams()[video_stream].encoding_name().to_owned();
        if !encoding.eq_ignore_ascii_case("h264") {
            anyhow::bail!(
                "this gateway carries H.264 without transcoding; the source offered {encoding}"
            );
        }
        tokio::time::timeout(
            Duration::from_secs(8),
            session.setup(
                video_stream,
                SetupOptions::default().frame_format(framing.into()),
            ),
        )
        .await
        .context("RTSP SETUP timeout")??;
        let playing =
            tokio::time::timeout(Duration::from_secs(8), session.play(PlayOptions::default()))
                .await
                .context("RTSP PLAY timeout")??;
        Ok(Self {
            demuxed: Box::pin(playing.demuxed()?),
            video_stream,
        })
    }
}

#[async_trait::async_trait]
impl FrameSource for RtspSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
        use futures::StreamExt as _;
        loop {
            let Some(item) = self.demuxed.next().await else {
                return Ok(None);
            };
            match item.context("receive RTSP media")? {
                CodecItem::VideoFrame(frame) if frame.stream_id() == self.video_stream => {
                    let timestamp = frame.timestamp();
                    return Ok(Some(Frame {
                        data: Bytes::copy_from_slice(frame.data()),
                        timestamp: timestamp.timestamp(),
                        clock_rate: timestamp.clock_rate().get(),
                        keyframe: frame.is_random_access_point(),
                        new_parameters: frame.has_new_parameters(),
                    }));
                }
                // Audio, ONVIF metadata, another stream: not ours.
                _ => continue,
            }
        }
    }

    fn parameters(&self) -> Option<VideoParameters> {
        match self.demuxed.streams()[self.video_stream].parameters() {
            Some(ParametersRef::Video(params)) => Some(VideoParameters {
                rfc6381_codec: params.rfc6381_codec().to_owned(),
                pixel_dimensions: params.pixel_dimensions(),
                extra_data: params.extra_data().to_vec(),
                frame_rate: params.frame_rate(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A source that yields what it was given: how a media path is tested
    /// without a camera, and the shape a pushed stream will take.
    pub struct ScriptedSource {
        pub frames: std::collections::VecDeque<Frame>,
        pub parameters: Option<VideoParameters>,
    }

    /// A source that never ends, for testing something that has to be stopped
    /// rather than waited out.
    pub struct EndlessSource {
        parameters: VideoParameters,
        index: i64,
    }

    impl EndlessSource {
        pub fn new(parameters: VideoParameters) -> Self {
            Self {
                parameters,
                index: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl FrameSource for EndlessSource {
        async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
            // Paced, so a test that forgets to stop it does not spin a core.
            tokio::time::sleep(Duration::from_millis(5)).await;
            let index = self.index;
            self.index += 1;
            Ok(Some(Frame {
                data: Bytes::from(vec![0_u8; 256]),
                timestamp: index * 3_600,
                clock_rate: 90_000,
                keyframe: index % 25 == 0,
                new_parameters: index == 0,
            }))
        }

        fn parameters(&self) -> Option<VideoParameters> {
            Some(self.parameters.clone())
        }
    }

    #[async_trait::async_trait]
    impl FrameSource for ScriptedSource {
        async fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
            Ok(self.frames.pop_front())
        }

        fn parameters(&self) -> Option<VideoParameters> {
            self.parameters.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_consumer_gets_the_framing_it_asks_for() {
        // The recorder's bytes and a live session's bytes are not the same bytes,
        // and neither is converted after the fact.
        assert_eq!(FrameFormat::from(Framing::Avcc), FrameFormat::MP4);
        assert_eq!(FrameFormat::from(Framing::AnnexB), FrameFormat::SIMPLE);
    }

    #[tokio::test]
    async fn a_source_that_is_not_rtsp_is_still_a_source() {
        use testing::ScriptedSource;
        let mut source = ScriptedSource {
            frames: [Frame {
                data: Bytes::from_static(b"\x00\x00\x00\x01frame"),
                timestamp: 900,
                clock_rate: 90_000,
                keyframe: true,
                new_parameters: true,
            }]
            .into_iter()
            .collect(),
            parameters: Some(VideoParameters {
                rfc6381_codec: "avc1.42e01e".into(),
                pixel_dimensions: (1920, 1080),
                extra_data: vec![1, 0x42, 0xe0, 0x1e, 0xff],
                frame_rate: Some((30, 1)),
            }),
        };
        let frame = source.next_frame().await.unwrap().expect("a frame");
        assert!(frame.keyframe);
        assert_eq!(frame.clock_rate, 90_000);
        assert_eq!(source.parameters().unwrap().pixel_dimensions, (1920, 1080));
        assert!(
            source.next_frame().await.unwrap().is_none(),
            "a source that has ended says so"
        );
    }
}
