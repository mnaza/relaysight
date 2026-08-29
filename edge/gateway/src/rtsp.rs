use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use futures::StreamExt;
use retina::{
    client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions},
    codec::CodecItem,
};
use tokio::time::timeout;
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct RtspMetrics {
    pub codec: Option<String>,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    pub packet_loss: u64,
    /// Counted while probing but not yet reported anywhere — the metrics
    /// endpoint that would expose them does not exist.
    #[allow(dead_code)]
    pub frames: u64,
    #[allow(dead_code)]
    pub bytes: u64,
}

pub async fn probe(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    sample_window: Duration,
) -> anyhow::Result<RtspMetrics> {
    let mut url = Url::parse(raw_url).context("parse RTSP URL")?;

    // Retina intentionally rejects credentials embedded in the URL. Accept them
    // from ONVIF/vendor responses, move them into SessionOptions, then scrub URL.
    let embedded_username = (!url.username().is_empty()).then(|| url.username().to_owned());
    let embedded_password = url.password().map(ToOwned::to_owned);
    if embedded_username.is_some() {
        url.set_username("")
            .map_err(|_| anyhow!("unable to clear RTSP username"))?;
    }
    if embedded_password.is_some() {
        url.set_password(None)
            .map_err(|_| anyhow!("unable to clear RTSP password"))?;
    }

    let username = username
        .map(ToOwned::to_owned)
        .or(embedded_username)
        .unwrap_or_default();
    let password = password
        .map(ToOwned::to_owned)
        .or(embedded_password)
        .unwrap_or_default();
    let creds = (!username.is_empty()).then_some(Credentials { username, password });

    let options = SessionOptions::default()
        .creds(creds)
        .user_agent(format!("vms-gateway/{}", env!("CARGO_PKG_VERSION")));

    let mut session = timeout(Duration::from_secs(8), Session::describe(url, options))
        .await
        .context("RTSP DESCRIBE timeout")??;

    let video_stream = session
        .streams()
        .iter()
        .position(|stream| stream.media().eq_ignore_ascii_case("video"))
        .ok_or_else(|| anyhow!("RTSP session has no video stream"))?;
    let codec = Some(session.streams()[video_stream].encoding_name().to_owned());

    timeout(
        Duration::from_secs(8),
        session.setup(video_stream, SetupOptions::default()),
    )
    .await
    .context("RTSP SETUP timeout")??;

    let playing = timeout(Duration::from_secs(8), session.play(PlayOptions::default()))
        .await
        .context("RTSP PLAY timeout")??;
    let mut demuxed = playing.demuxed()?;

    let started = Instant::now();
    let deadline = started + sample_window;
    let mut frames = 0_u64;
    let mut bytes = 0_u64;
    let mut packet_loss = 0_u64;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining.min(Duration::from_secs(3)), demuxed.next()).await {
            Ok(Some(Ok(CodecItem::VideoFrame(frame)))) => {
                if frame.stream_id() == video_stream {
                    frames += 1;
                    bytes += frame.data().len() as u64;
                    packet_loss += u64::from(frame.loss());
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => return Err(error).context("receive RTSP media"),
            Ok(None) => break,
            Err(_) => {
                if frames == 0 {
                    return Err(anyhow!("RTSP stream produced no video frames"));
                }
                break;
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    if frames == 0 {
        return Err(anyhow!("RTSP stream produced no video frames"));
    }

    Ok(RtspMetrics {
        codec,
        fps: Some((frames as f64 / elapsed) as f32),
        bitrate_kbps: Some(((bytes as f64 * 8.0 / elapsed) / 1000.0).round() as u32),
        packet_loss,
        frames,
        bytes,
    })
}

pub fn redacted_endpoint(raw_url: &str) -> Option<String> {
    let mut url = Url::parse(raw_url).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url.to_string())
}
