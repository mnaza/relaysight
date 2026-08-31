//! A WebRTC peer standing in for the browser, for tests.
//!
//! `live::start_h264` is the one path that could not be reached from either side:
//! the fake camera covers its RTSP half, but nothing drove the peer connection,
//! so SDP negotiation, ICE on loopback and the H.264 sample writer were untested.
//! This is the other half — it offers exactly what a browser offers, accepts the
//! gateway's answer, and counts what actually arrives on the wire.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, anyhow};
use rtc::{
    interceptor::Registry,
    peer_connection::{
        configuration::{
            RTCConfigurationBuilder,
            interceptor_registry::register_default_interceptors,
            media_engine::{MIME_TYPE_H264, MediaEngine},
        },
        sdp::RTCSessionDescription,
    },
    rtp_transceiver::{
        RTCRtpTransceiverDirection, RTCRtpTransceiverInit,
        rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind},
    },
};
use tokio::sync::Notify;
use webrtc::{
    media_stream::track_remote::{TrackRemote, TrackRemoteEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    },
    runtime::TokioRuntime,
};

/// What actually came down the track. Counting bytes as well as packets keeps a
/// test from passing on empty padding.
#[derive(Debug, Default)]
pub struct Received {
    pub packets: u64,
    pub payload_bytes: u64,
}

#[derive(Clone)]
struct Handler {
    gather_complete: Arc<Notify>,
    got_media: Arc<Notify>,
    packets: Arc<AtomicU64>,
    payload_bytes: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather_complete.notify_waiters();
            self.gather_complete.notify_one();
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let packets = Arc::clone(&self.packets);
        let payload_bytes = Arc::clone(&self.payload_bytes);
        let got_media = Arc::clone(&self.got_media);
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    packets.fetch_add(1, Ordering::Relaxed);
                    payload_bytes.fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
                    got_media.notify_waiters();
                    got_media.notify_one();
                }
            }
        });
    }
}

pub struct FakeBrowser {
    peer: Arc<dyn PeerConnection>,
    offer_sdp: String,
    got_media: Arc<Notify>,
    packets: Arc<AtomicU64>,
    payload_bytes: Arc<AtomicU64>,
}

impl FakeBrowser {
    /// Build a recvonly H.264 offer with ICE already gathered, the way the
    /// gateway expects it — it does not trickle, so neither does this.
    pub async fn offer() -> anyhow::Result<Self> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
            },
            RtpCodecKind::Video,
        )?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        let gather_complete = Arc::new(Notify::new());
        let got_media = Arc::new(Notify::new());
        let packets = Arc::new(AtomicU64::new(0));
        let payload_bytes = Arc::new(AtomicU64::new(0));
        let handler = Arc::new(Handler {
            gather_complete: Arc::clone(&gather_complete),
            got_media: Arc::clone(&got_media),
            packets: Arc::clone(&packets),
            payload_bytes: Arc::clone(&payload_bytes),
        });

        let peer = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(Arc::new(TokioRuntime))
            .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
            .build()
            .await?;
        let peer: Arc<dyn PeerConnection> = Arc::new(peer);

        peer.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
                streams: vec![],
            }),
        )
        .await?;

        let offer = peer.create_offer(None).await?;
        peer.set_local_description(offer).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gather_complete.notified(),
        )
        .await
        .context("browser ICE gathering timeout")?;

        let local = peer
            .local_description()
            .await
            .ok_or_else(|| anyhow!("browser has no local description"))?;
        let offer_sdp = serde_json::to_value(&local)?
            .get("sdp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("browser offer has no SDP"))?
            .to_owned();

        Ok(Self {
            peer,
            offer_sdp,
            got_media,
            packets,
            payload_bytes,
        })
    }

    /// The path this peer settled on, read with the same code the gateway uses.
    /// On loopback it must be `host`; anything else means the extraction is wrong.
    pub async fn path(&self) -> crate::icepath::PathKind {
        crate::icepath::observed(&self.peer).await
    }

    pub fn offer_sdp(&self) -> &str {
        &self.offer_sdp
    }

    pub async fn accept_answer(&self, sdp: &str) -> anyhow::Result<()> {
        let answer: RTCSessionDescription =
            serde_json::from_value(serde_json::json!({"type": "answer", "sdp": sdp}))
                .context("parse gateway answer")?;
        self.peer.set_remote_description(answer).await?;
        Ok(())
    }

    /// Wait until media actually arrives, then let a little more accumulate.
    pub async fn wait_for_media(&self, timeout: std::time::Duration) -> anyhow::Result<Received> {
        tokio::time::timeout(timeout, self.got_media.notified())
            .await
            .context("no RTP arrived from the gateway")?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        Ok(Received {
            packets: self.packets.load(Ordering::Relaxed),
            payload_bytes: self.payload_bytes.load(Ordering::Relaxed),
        })
    }
}
