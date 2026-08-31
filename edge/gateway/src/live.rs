use std::{sync::Arc, time::Duration};

use anyhow::{Context, anyhow};
use bytes::Bytes;
use futures::StreamExt;
use retina::{
    client::{PlayOptions, Session, SessionOptions, SetupOptions},
    codec::{CodecItem, FrameFormat},
};
use rtc::{
    interceptor::Registry,
    media::Sample,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            RTCConfigurationBuilder,
            interceptor_registry::register_default_interceptors,
            media_engine::{MIME_TYPE_H264, MediaEngine},
        },
        sdp::RTCSessionDescription,
        transport::RTCIceServer,
    },
    rtp_transceiver::{
        PayloadType,
        rtp_sender::{
            RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
            RtpCodecKind,
        },
    },
};
use tokio::sync::Notify;
use tracing::{info, warn};
use vms_domain::{LiveSessionAnswer, RtcIceServerConfig};
use webrtc::media_stream::Track;
use webrtc::{
    media_stream::track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        RTCPeerConnectionState,
    },
    rtp_transceiver::RtpSender,
    runtime::TokioRuntime,
};

#[derive(Clone)]
struct Handler {
    gather_complete: Arc<Notify>,
    connected: Arc<Notify>,
    done: Arc<Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather_complete.notify_waiters();
            self.gather_complete.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        match state {
            RTCPeerConnectionState::Connected => {
                self.connected.notify_waiters();
                self.connected.notify_one();
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                self.done.notify_waiters();
                self.done.notify_one();
            }
            _ => {}
        }
    }
}

pub async fn start_h264(
    rtsp_uri: String,
    username: Option<String>,
    password: Option<String>,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
) -> anyhow::Result<LiveSessionAnswer> {
    if !offer_type.eq_ignore_ascii_case("offer") {
        anyhow::bail!("unsupported SDP type {offer_type}; expected offer");
    }

    let mut media_engine = MediaEngine::default();
    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    let rtc_servers = ice_servers
        .into_iter()
        .map(|server| RTCIceServer {
            urls: server.urls,
            username: server.username,
            credential: server.credential,
        })
        .collect();
    let rtc_config = RTCConfigurationBuilder::new()
        .with_ice_servers(rtc_servers)
        .build();

    let gather_complete = Arc::new(Notify::new());
    let connected = Arc::new(Notify::new());
    let done = Arc::new(Notify::new());
    let handler = Arc::new(Handler {
        gather_complete: gather_complete.clone(),
        connected: connected.clone(),
        done: done.clone(),
    });

    let peer = PeerConnectionBuilder::new()
        .with_configuration(rtc_config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(Arc::new(TokioRuntime))
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await?;
    let peer: Arc<dyn PeerConnection> = Arc::new(peer);

    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        format!("camera-{ssrc}"),
        format!("video-{ssrc}"),
        "camera-video".into(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);
    let sender = peer
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;

    let offer: RTCSessionDescription = serde_json::from_value(serde_json::json!({
        "type": offer_type,
        "sdp": offer_sdp,
    }))
    .context("parse browser SDP offer")?;
    peer.set_remote_description(offer).await?;
    let answer = peer.create_answer(None).await?;
    peer.set_local_description(answer).await?;

    tokio::time::timeout(Duration::from_secs(12), gather_complete.notified())
        .await
        .context("WebRTC ICE gathering timeout")?;
    let local = peer
        .local_description()
        .await
        .ok_or_else(|| anyhow!("WebRTC local description is missing"))?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(i64::from(session_seconds));
    let answer_json = serde_json::to_value(&local)?;
    let sdp = answer_json
        .get("sdp")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WebRTC answer has no SDP"))?
        .to_owned();
    let sdp_type = answer_json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("answer")
        .to_owned();

    let peer_task = peer.clone();
    let peer_stats = peer.clone();
    let task_session_id = session_id.clone();
    tokio::spawn(async move {
        let stream = async {
            tokio::time::timeout(Duration::from_secs(20), connected.notified())
                .await
                .context("WebRTC peer did not connect")?;
            // Log the path this session settled on. Relayed sessions are the ones
            // that cost bandwidth, and their share decides the hosting model — see
            // docs/TURN-COSTS.md. It cannot be guessed, so every session says.
            let path = crate::icepath::observed(&peer_stats).await;
            info!(
                session_id = %task_session_id,
                path = %path,
                relayed = path.is_relayed(),
                "WebRTC peer connected; starting RTSP forwarding"
            );
            forward_rtsp_h264(
                rtsp_uri,
                username,
                password,
                track,
                sender,
                done.clone(),
                Duration::from_secs(u64::from(session_seconds)),
            )
            .await
        };
        if let Err(error) = stream.await {
            warn!(session_id = %task_session_id, %error, "WebRTC live session ended with error");
        }
        let _ = peer_task.close().await;
    });

    Ok(LiveSessionAnswer {
        session_id,
        sdp,
        sdp_type,
        codec: "H264".into(),
        expires_at,
    })
}

async fn negotiated_payload_type(sender: &Arc<dyn RtpSender>) -> anyhow::Result<PayloadType> {
    sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .ok_or_else(|| anyhow!("WebRTC sender has no negotiated H264 codec"))
}

async fn forward_rtsp_h264(
    raw_url: String,
    username: Option<String>,
    password: Option<String>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    done: Arc<Notify>,
    max_duration: Duration,
) -> anyhow::Result<()> {
    let (url, creds) =
        crate::rtsp::split_credentials(&raw_url, username.as_deref(), password.as_deref())?;

    let mut session = tokio::time::timeout(
        Duration::from_secs(8),
        Session::describe(
            url,
            SessionOptions::default()
                .creds(creds)
                .user_agent(format!("vms-gateway/{}", env!("CARGO_PKG_VERSION"))),
        ),
    )
    .await
    .context("RTSP DESCRIBE timeout")??;
    let video_stream = session
        .streams()
        .iter()
        .position(|stream| stream.media().eq_ignore_ascii_case("video"))
        .ok_or_else(|| anyhow!("RTSP live source has no video stream"))?;
    let encoding = session.streams()[video_stream].encoding_name();
    if !encoding.eq_ignore_ascii_case("h264") {
        anyhow::bail!("zero-transcode live currently requires H264, camera returned {encoding}");
    }
    tokio::time::timeout(
        Duration::from_secs(8),
        session.setup(
            video_stream,
            SetupOptions::default().frame_format(FrameFormat::SIMPLE),
        ),
    )
    .await
    .context("RTSP SETUP timeout")??;
    let playing =
        tokio::time::timeout(Duration::from_secs(8), session.play(PlayOptions::default()))
            .await
            .context("RTSP PLAY timeout")??;
    let mut demuxed = playing.demuxed()?;
    let payload_type = negotiated_payload_type(&sender).await?;
    let track_ssrc = *track
        .ssrcs()
        .await
        .first()
        .ok_or_else(|| anyhow!("WebRTC track has no SSRC"))?;
    let deadline = tokio::time::sleep(max_duration);
    tokio::pin!(deadline);
    let mut started = false;
    let mut pending: Option<(Bytes, retina::Timestamp)> = None;

    loop {
        tokio::select! {
            _ = done.notified() => break,
            _ = &mut deadline => break,
            item = demuxed.next() => match item {
                Some(Ok(CodecItem::VideoFrame(frame))) if frame.stream_id() == video_stream => {
                    if !started {
                        if !frame.is_random_access_point() { continue; }
                        started = true;
                    }
                    let timestamp = frame.timestamp();
                    let next_data = Bytes::copy_from_slice(frame.data());
                    if let Some((data, previous_timestamp)) = pending.replace((next_data, timestamp)) {
                        let ticks = timestamp
                            .timestamp()
                            .saturating_sub(previous_timestamp.timestamp());
                        let duration = crate::rtsp::frame_duration(
                            ticks,
                            previous_timestamp.clock_rate().get(),
                        );
                        track.sample_writer(track_ssrc, payload_type).write_sample(&Sample {
                            data, duration, ..Default::default()
                        }).await?;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("receive RTSP live media"),
                None => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{fake_browser::FakeBrowser, fake_camera::FakeCamera};

    #[tokio::test]
    async fn h264_reaches_a_browser_peer_from_a_real_camera() {
        // The whole live path in one go: RTSP session with the camera, SDP
        // negotiation with the browser, ICE on loopback, and H.264 forwarded as
        // encoded samples without transcoding. Every earlier test stopped short
        // of the peer connection.
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();

        let answer = super::start_h264(
            camera.url.clone(),
            None,
            None,
            browser.offer_sdp().to_owned(),
            "offer".into(),
            Vec::new(),
            10,
        )
        .await
        .expect("gateway answers the offer");

        assert_eq!(answer.sdp_type, "answer");
        assert_eq!(answer.codec, "H264");
        assert!(
            answer.sdp.contains("m=video"),
            "the answer must carry a video section"
        );

        browser.accept_answer(&answer.sdp).await.unwrap();
        let received = browser
            .wait_for_media(Duration::from_secs(15))
            .await
            .expect("media must arrive");

        // The path is read with the same code the gateway logs from. On loopback
        // it has to be a host pair, so anything else means the extraction picked
        // the wrong candidate — and that number feeds a cost model.
        let path = browser.path().await;
        assert_eq!(
            path,
            crate::icepath::PathKind::Host,
            "loopback session reported {path}, so the candidate lookup is wrong"
        );
        assert!(!path.is_relayed());

        assert!(received.packets > 0, "no RTP packets arrived");
        assert!(
            received.payload_bytes > 1000,
            "only {} payload bytes arrived, which is padding rather than video",
            received.payload_bytes
        );
    }

    #[tokio::test]
    async fn an_answer_is_refused_as_an_offer() {
        // The command carries the SDP type from the browser. Treating an answer
        // as an offer would produce an unusable session rather than an error.
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let error = super::start_h264(
            camera.url.clone(),
            None,
            None,
            browser.offer_sdp().to_owned(),
            "answer".into(),
            Vec::new(),
            10,
        )
        .await
        .expect_err("an answer is not an offer");
        assert!(error.to_string().contains("expected offer"));
    }

    #[tokio::test]
    async fn a_camera_that_refuses_the_session_does_not_leave_a_live_answer_pending() {
        // The gateway answers the browser before it touches the camera, so an
        // unreachable camera still produces an answer — but no media. Pinning
        // this stops a later change from quietly making the answer conditional
        // and breaking the browser's negotiation.
        let camera = FakeCamera::start(true).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let answer = super::start_h264(
            camera.url.clone(),
            None,
            None,
            browser.offer_sdp().to_owned(),
            "offer".into(),
            Vec::new(),
            3,
        )
        .await
        .expect("the answer does not depend on the camera");
        browser.accept_answer(&answer.sdp).await.unwrap();
        assert!(
            browser
                .wait_for_media(Duration::from_secs(3))
                .await
                .is_err(),
            "media must not arrive from a camera that refused the session"
        );
    }
}
