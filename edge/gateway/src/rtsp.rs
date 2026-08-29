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

/// Lift credentials out of an RTSP URL and scrub it.
///
/// Retina deliberately rejects credentials embedded in the URL, but ONVIF and
/// vendor responses hand them back that way, so they must be moved into the
/// session options. Explicitly configured credentials win over embedded ones.
///
/// Returns `None` for the credentials when there are none, rather than an empty
/// pair — a camera permitting anonymous access rejects a session that offers an
/// empty username.
/// Split a URL's embedded userinfo away from the URL itself.
///
/// Shared by every path that talks to a camera — RTSP live, RTSP archive and the
/// HTTP snapshot — because each receives URLs from ONVIF or vendor responses that
/// may carry credentials inline, and none of them may pass such a URL onward.
/// Returns the scrubbed URL and whatever was embedded.
pub fn strip_userinfo(
    raw_url: &str,
    context_label: &str,
) -> anyhow::Result<(Url, Option<String>, Option<String>)> {
    let mut url = Url::parse(raw_url).with_context(|| format!("parse {context_label} URL"))?;
    // A URL that cannot be a base — "admin:pw@junk" parses as scheme `admin` with
    // an opaque path — reports an empty username, so the clearing below never
    // runs and the credentials ride along inside the string. No camera URL is
    // ever of that shape, so reject it rather than pass it on.
    if url.cannot_be_a_base() {
        anyhow::bail!("{context_label} URL is not a hierarchical URL");
    }
    let embedded_username = (!url.username().is_empty()).then(|| url.username().to_owned());
    let embedded_password = url.password().map(ToOwned::to_owned);
    if embedded_username.is_some() {
        url.set_username("")
            .map_err(|_| anyhow!("unable to clear {context_label} username"))?;
    }
    if embedded_password.is_some() {
        url.set_password(None)
            .map_err(|_| anyhow!("unable to clear {context_label} password"))?;
    }
    Ok((url, embedded_username, embedded_password))
}

/// Lift credentials out of an RTSP URL and scrub it.
///
/// Retina deliberately rejects credentials embedded in the URL, but ONVIF and
/// vendor responses hand them back that way, so they must be moved into the
/// session options. Explicitly configured credentials win over embedded ones.
///
/// Returns `None` for the credentials when there are none, rather than an empty
/// pair — a camera permitting anonymous access rejects a session that offers an
/// empty username.
pub fn split_credentials(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<(Url, Option<Credentials>)> {
    let (url, embedded_username, embedded_password) = strip_userinfo(raw_url, "RTSP")?;
    let user = username
        .map(ToOwned::to_owned)
        .or(embedded_username)
        .unwrap_or_default();
    let pass = password
        .map(ToOwned::to_owned)
        .or(embedded_password)
        .unwrap_or_default();
    let creds = (!user.is_empty()).then_some(Credentials {
        username: user,
        password: pass,
    });
    Ok((url, creds))
}

/// Sample duration from the gap between two RTP timestamps.
///
/// Non-advancing or backwards gaps happen on retransmits and reordering; a fixed
/// 30 fps guess is better there than a zero-length sample. The clamp stops a
/// stalled camera producing a half-second-plus sample that wedges the player, and
/// a burst producing a zero-length one.
pub fn frame_duration(ticks: i64, clock_rate: u32) -> Duration {
    const FALLBACK: Duration = Duration::from_millis(33);
    if ticks <= 0 || clock_rate == 0 {
        return FALLBACK;
    }
    Duration::from_secs_f64((ticks as f64 / f64::from(clock_rate)).clamp(0.005, 0.5))
}

pub async fn probe(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    sample_window: Duration,
) -> anyhow::Result<RtspMetrics> {
    let (url, creds) = split_credentials(raw_url, username, password)?;

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
    // Fail closed. This string goes to the cloud as telemetry and into logs, so
    // emitting nothing beats emitting something that still carries a password.
    // Before this went through `strip_userinfo` it discarded the errors from
    // `set_username`/`set_password` and published such URLs verbatim.
    let (url, _, _) = strip_userinfo(raw_url, "telemetry").ok()?;
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{frame_duration, redacted_endpoint, split_credentials, strip_userinfo};
    use std::time::Duration;

    #[test]
    fn redaction_removes_both_halves_of_the_userinfo() {
        // This string goes to the cloud as telemetry and into logs. A camera
        // password reaching either is a credential leak, not a cosmetic bug.
        let out = redacted_endpoint("rtsp://admin:hunter2@10.0.0.5:554/Streaming/Channels/101")
            .expect("parseable");
        assert!(
            !out.contains("hunter2"),
            "password survived redaction: {out}"
        );
        assert!(!out.contains("admin"), "username survived redaction: {out}");
        assert_eq!(out, "rtsp://10.0.0.5:554/Streaming/Channels/101");
    }

    #[test]
    fn redaction_keeps_a_url_that_had_no_credentials_intact() {
        assert_eq!(
            redacted_endpoint("rtsp://10.0.0.5/stream").unwrap(),
            "rtsp://10.0.0.5/stream"
        );
    }

    #[test]
    fn redaction_emits_nothing_rather_than_something_unredacted() {
        // Failing closed matters more than reporting an endpoint.
        assert!(redacted_endpoint("admin:hunter2@not a url").is_none());
    }

    #[test]
    fn embedded_credentials_move_out_of_the_url() {
        let (url, creds) =
            split_credentials("rtsp://admin:hunter2@10.0.0.5/stream", None, None).unwrap();
        assert_eq!(url.as_str(), "rtsp://10.0.0.5/stream");
        let creds = creds.expect("credentials lifted");
        assert_eq!(creds.username, "admin");
        assert_eq!(creds.password, "hunter2");
    }

    #[test]
    fn explicit_credentials_win_over_embedded_ones() {
        let (url, creds) = split_credentials(
            "rtsp://olduser:oldpass@10.0.0.5/stream",
            Some("configured"),
            Some("secret"),
        )
        .unwrap();
        assert_eq!(url.as_str(), "rtsp://10.0.0.5/stream");
        let creds = creds.unwrap();
        assert_eq!(creds.username, "configured");
        assert_eq!(creds.password, "secret");
    }

    #[test]
    fn no_credentials_anywhere_yields_none_not_an_empty_pair() {
        // Offering an empty username makes a camera that allows anonymous
        // access reject the session.
        let (_, creds) = split_credentials("rtsp://10.0.0.5/stream", None, None).unwrap();
        assert!(creds.is_none());
    }

    #[test]
    fn frame_duration_tracks_the_rtp_clock() {
        // 3000 ticks of a 90 kHz clock is one frame at 30 fps.
        assert_eq!(
            frame_duration(3000, 90_000),
            Duration::from_secs_f64(1.0 / 30.0)
        );
    }

    #[test]
    fn frame_duration_falls_back_when_the_clock_does_not_advance() {
        // Equal or backwards timestamps happen on retransmits and reordering.
        assert_eq!(frame_duration(0, 90_000), Duration::from_millis(33));
        assert_eq!(frame_duration(-9000, 90_000), Duration::from_millis(33));
    }

    #[test]
    fn frame_duration_is_clamped_at_both_ends() {
        assert_eq!(frame_duration(90, 90_000), Duration::from_millis(5));
        assert_eq!(
            frame_duration(90_000 * 30, 90_000),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn frame_duration_survives_a_zero_clock_rate() {
        // Guarding this is cheaper than trusting every camera's SDP.
        assert_eq!(frame_duration(3000, 0), Duration::from_millis(33));
    }

    #[test]
    fn strip_userinfo_scrubs_the_url_for_every_camera_path() {
        // Live, archive and the HTTP snapshot all take URLs straight from ONVIF
        // responses. One implementation, so a fix here fixes all three.
        let (url, user, pass) =
            strip_userinfo("http://admin:hunter2@10.0.0.5/snapshot.jpg", "test").unwrap();
        assert_eq!(url.as_str(), "http://10.0.0.5/snapshot.jpg");
        assert_eq!(user.as_deref(), Some("admin"));
        assert_eq!(pass.as_deref(), Some("hunter2"));
    }

    #[test]
    fn strip_userinfo_refuses_a_url_whose_userinfo_cannot_be_cleared() {
        // The same shape that made redacted_endpoint leak: parses fine, cannot
        // hold a host, so clearing fails. Erroring beats passing it through.
        assert!(strip_userinfo("admin:hunter2@junk", "test").is_err());
    }
}
