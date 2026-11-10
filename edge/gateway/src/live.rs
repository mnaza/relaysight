use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use bytes::Bytes;
use rtc::{
    interceptor::Registry,
    media::Sample,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            RTCConfigurationBuilder, RTCIceTransportPolicy,
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
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceCandidateType,
        RTCIceGatheringState, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    },
    rtp_transceiver::RtpSender,
    runtime::TokioRuntime,
};

#[derive(Clone)]
struct Handler {
    gather_complete: Arc<Notify>,
    gathering: Arc<Gathering>,
    connected: Arc<Notify>,
    done: Arc<Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        // The marker for "that is everything" arrives as a candidate with no
        // address, and it is not a kind anybody can wait for.
        if !event.candidate.address.is_empty() {
            self.gathering.record(event.candidate.typ);
        }
    }

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

/// Where a live session's frames come from: a camera this gateway dials when
/// the browser connects, or a stream already being pushed to it.
pub enum LiveMedia {
    Rtsp {
        url: String,
        username: Option<String>,
        password: Option<String>,
    },
    Pushed(Box<dyn crate::frames::FrameSource>),
}

impl LiveMedia {
    async fn open(self) -> anyhow::Result<Box<dyn crate::frames::FrameSource>> {
        match self {
            // Dialled late on purpose: a camera should not be streaming while
            // the browser is still gathering candidates.
            LiveMedia::Rtsp {
                url,
                username,
                password,
            } => Ok(Box::new(
                crate::frames::RtspSource::open(
                    &url,
                    username.as_deref(),
                    password.as_deref(),
                    crate::frames::Framing::AnnexB,
                )
                .await?,
            )),
            LiveMedia::Pushed(source) => Ok(source),
        }
    }
}

/// What the ICE servers a session was given can be expected to produce.
///
/// Host candidates are deliberately not on this list. They come from local
/// sockets, all of them queued before any server is asked, so there is nothing
/// to wait for — and treating "a host candidate arrived" as progress would mean
/// answering before the second interface's was added.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct ServerKinds {
    srflx: bool,
    relay: bool,
}

impl ServerKinds {
    /// Nothing is owed: no server was configured, or none that produces
    /// anything.
    fn nothing(self) -> bool {
        !self.srflx && !self.relay
    }

    /// Whether everything `owed` promises has arrived.
    fn covers(self, owed: ServerKinds) -> bool {
        (!owed.srflx || self.srflx) && (!owed.relay || self.relay)
    }

    /// What `owed` promises and this does not have, for the log.
    fn missing_from(self, owed: ServerKinds) -> Vec<&'static str> {
        [
            ("srflx", owed.srflx, self.srflx),
            ("relay", owed.relay, self.relay),
        ]
        .into_iter()
        .filter(|(_, owed, have)| *owed && !*have)
        .map(|(name, _, _)| name)
        .collect()
    }

    fn record(&mut self, kind: RTCIceCandidateType) {
        match kind {
            RTCIceCandidateType::Srflx => self.srflx = true,
            RTCIceCandidateType::Relay => self.relay = true,
            // Host candidates are not waited for (above), and a peer-reflexive
            // one is discovered by the other end during connectivity checks.
            _ => {}
        }
    }
}

/// What the servers in an ICE configuration can produce.
///
/// A `stun:` URL can produce a server-reflexive candidate and a
/// `turn:`/`turns:` URL a relayed one, and one of each is all a session can
/// use: a second interface asking the same server produces the same kind
/// again, which is the whole of why a session holding one of each has nothing
/// left to wait for while sockets that can reach nothing retransmit.
///
/// Reflexive candidates are expected from `stun:` URLs only because that is
/// all this library's gatherer asks for
/// (`webrtc::peer_connection::transports::stun_gatherer`): expecting one from
/// a relay would be waiting for something nothing is going to produce.
fn kinds_to_expect(urls: &[String], policy: Option<RTCIceTransportPolicy>) -> ServerKinds {
    // A session pinned to the relay gathers nothing else, by design.
    let relay_only = policy == Some(RTCIceTransportPolicy::Relay);
    let mut kinds = ServerKinds::default();
    for url in urls {
        let url = url.trim().to_ascii_lowercase();
        if url.starts_with("stun:") && !relay_only {
            kinds.srflx = true;
        }
        if url.starts_with("turn:") || url.starts_with("turns:") {
            kinds.relay = true;
        }
    }
    kinds
}

/// What a session is waiting for while it gathers, and what has arrived.
///
/// A gateway binds one UDP socket per interface and sends a STUN binding
/// request from each, and a host with Docker bridges has several that can
/// reach nothing: those requests are retransmitted until the session's budget
/// is gone (relaysight-vms-0kem). They could only ever repeat a kind already
/// in hand, so the session stops waiting as soon as it holds one of each kind
/// this configuration can produce.
struct Gathering {
    expected: ServerKinds,
    gathered: std::sync::Mutex<ServerKinds>,
    enough: Notify,
}

impl Gathering {
    fn new(expected: ServerKinds) -> Self {
        Self {
            expected,
            gathered: std::sync::Mutex::new(ServerKinds::default()),
            enough: Notify::new(),
        }
    }

    /// Records a gathered candidate, waking the session once there is one of
    /// every kind the servers can produce.
    fn record(&self, kind: RTCIceCandidateType) {
        // A session with no servers to wait for is already as gathered as it
        // will ever be, and the library says so in the same breath.
        if self.expected.nothing() {
            return;
        }
        let mut gathered = self.gathered.lock().expect("gathered candidate kinds");
        gathered.record(kind);
        if gathered.covers(self.expected) {
            self.enough.notify_waiters();
            self.enough.notify_one();
        }
    }

    /// What never arrived, for the log when the budget runs out.
    fn missing(&self) -> Vec<&'static str> {
        self.gathered
            .lock()
            .expect("gathered candidate kinds")
            .missing_from(self.expected)
    }
}

/// How gathering ended, which decides what the log says and whether the answer
/// has to declare its candidates final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gathered {
    /// The library is done: every socket answered or was given up on.
    Complete,
    /// There is one candidate of every kind this configuration can produce,
    /// and whatever is still outstanding can only repeat them.
    Enough,
    /// The budget ran out. Whatever is in hand is what the session gets.
    OutOfTime,
}

/// Says in the SDP that the candidates it carries are all there will be.
///
/// This answer is the whole of the gateway's signalling — there is no channel
/// to trickle a late candidate down — so a browser left without this waits for
/// more before it gives up on a pair, and shows nothing while it waits. Added
/// at the end because a session like this has exactly one media section, which
/// is the one it belongs to.
fn declare_the_candidates_final(sdp: &str) -> String {
    if sdp.contains("a=end-of-candidates") {
        return sdp.to_owned();
    }
    let mut sdp = sdp.to_owned();
    if !sdp.ends_with("\r\n") {
        sdp.push_str("\r\n");
    }
    sdp.push_str("a=end-of-candidates\r\n");
    sdp
}

/// The refusal for a session the gateway has no way to carry at all.
///
/// It used to say "WebRTC ICE gathering timeout", which names the symptom and
/// nothing an operator can go and check.
fn no_path_refusal(urls: &[String], budget: Duration) -> String {
    let servers = if urls.is_empty() {
        "none were configured".to_owned()
    } else {
        urls.join(", ")
    };
    format!(
        "the gateway gathered no network path in {}s, so it has no way to carry this session: \
         no local address was usable and no ICE server answered ({servers}). Check that this \
         host has an interface the browser can reach, and that it can send UDP to those \
         servers; RTC_UDP_ADDRS on the gateway pins which interfaces to offer, and the control \
         plane's RTC_STUN_URLS which servers to ask",
        budget.as_secs()
    )
}

/// How long a session may spend gathering candidates before it is answered
/// with whatever it has.
const GATHERING_BUDGET: Duration = Duration::from_secs(12);

/// The local addresses the gateway binds for media: `RTC_UDP_ADDRS`, or every
/// interface the host has when it is unset.
///
/// Pinning it is how a host whose interfaces are not all usable keeps the
/// useless ones out of its SDP. The machine this was found on has five Docker
/// bridges (relaysight-vms-0kem): a browser dutifully pairs with every
/// candidate it is sent, so each one is a connectivity check that cannot
/// succeed — and a line in an SDP naming a site's internal network.
pub(crate) fn media_udp_addrs() -> Vec<String> {
    static VALUE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    VALUE
        .get_or_init(|| media_udp_addrs_from(std::env::var("RTC_UDP_ADDRS").ok().as_deref()))
        .clone()
}

/// Split out so the parsing can be tested without the environment. A bare
/// address gets port 0: an operator naming an interface means "this address",
/// not "this port".
fn media_udp_addrs_from(value: Option<&str>) -> Vec<String> {
    let addrs: Vec<String> = value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part.parse::<std::net::SocketAddr>().is_ok() {
                part.to_owned()
            } else if part.parse::<std::net::IpAddr>().is_ok() {
                format!("{part}:0")
            } else {
                part.to_owned()
            }
        })
        .collect();
    if addrs.is_empty() {
        vec!["0.0.0.0:0".to_owned()]
    } else {
        addrs
    }
}

/// The knobs production does not touch.
///
/// `start_h264` builds this from the environment; a test sets the one thing it
/// needs in order to drive a case that would otherwise need a particular host —
/// a STUN server that never answers, a budget short enough to expire, a single
/// interface to bind.
pub(crate) struct LiveTuning {
    /// Limits which candidates are gathered; the relay check pins it to `Relay`.
    pub(crate) ice_transport_policy: Option<RTCIceTransportPolicy>,
    /// The local addresses to bind for media.
    pub(crate) udp_addrs: Vec<String>,
    /// How long gathering may take before the session is answered anyway.
    pub(crate) gathering_budget: Duration,
}

impl Default for LiveTuning {
    fn default() -> Self {
        Self {
            ice_transport_policy: None,
            udp_addrs: media_udp_addrs(),
            gathering_budget: GATHERING_BUDGET,
        }
    }
}

pub async fn start_h264(
    media: LiveMedia,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
) -> anyhow::Result<LiveSessionAnswer> {
    start_h264_with(
        media,
        offer_sdp,
        offer_type,
        ice_servers,
        session_seconds,
        LiveTuning::default(),
    )
    .await
    .map(|(answer, _)| answer)
}

/// `start_h264` with the knobs exposed, also returning the counters of any TURN
/// bridges the session uses. Production passes `LiveTuning::default()`.
pub(crate) async fn start_h264_with(
    media: LiveMedia,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
    tuning: LiveTuning,
) -> anyhow::Result<(LiveSessionAnswer, Vec<Arc<crate::turn_bridge::BridgeStats>>)> {
    if !offer_type.eq_ignore_ascii_case("offer") {
        anyhow::bail!("unsupported SDP type {offer_type}; expected offer");
    }

    refuse_unless_the_browser_can_receive_h264(&offer_sdp)?;

    let mut media_engine = MediaEngine::default();
    // One profile is enough: codec matching falls back to the mime type when no
    // fmtp matches exactly, so a browser offering only 42001f or 640c1f still
    // negotiates against this and gets the payload type it chose. What it does
    // not survive is a browser with no H.264 at all, which is what the check
    // above is for.
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
    // Relays this site cannot reach over UDP go through local TLS/TCP bridges;
    // the guard keeps them running for the life of the session.
    let (ice_servers, bridges) = crate::turn_bridge::plan(ice_servers).await;
    let bridge_stats = bridges.stats();
    // What this session can gather, and what to name if it gathers nothing.
    let urls: Vec<String> = ice_servers
        .iter()
        .flat_map(|server| server.urls.iter().cloned())
        .collect();
    let rtc_servers = ice_servers
        .into_iter()
        .map(|server| RTCIceServer {
            urls: server.urls,
            username: server.username,
            credential: server.credential,
        })
        .collect();
    let mut rtc_config = RTCConfigurationBuilder::new().with_ice_servers(rtc_servers);
    if let Some(policy) = tuning.ice_transport_policy {
        rtc_config = rtc_config.with_ice_transport_policy(policy);
    }
    let rtc_config = rtc_config.build();

    let gather_complete = Arc::new(Notify::new());
    let gathering = Arc::new(Gathering::new(kinds_to_expect(
        &urls,
        tuning.ice_transport_policy,
    )));
    let connected = Arc::new(Notify::new());
    let done = Arc::new(Notify::new());
    let handler = Arc::new(Handler {
        gather_complete: gather_complete.clone(),
        gathering: gathering.clone(),
        connected: connected.clone(),
        done: done.clone(),
    });

    let peer = PeerConnectionBuilder::new()
        .with_configuration(rtc_config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(Arc::new(TokioRuntime))
        .with_udp_addrs(tuning.udp_addrs.clone())
        .build()
        .await?;
    let peer: Arc<dyn PeerConnection> = Arc::new(peer);

    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
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
        ),
    )?);
    let sender = peer
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;

    let offer: RTCSessionDescription = serde_json::from_value(serde_json::json!({
        "type": offer_type,
        "sdp": offer_sdp,
    }))
    .context("parse browser SDP offer")?;
    // What the browser brought. When a session does not connect, this is the
    // first thing worth knowing and the hardest to get afterwards: a browser
    // that gathered nothing, or only names this gateway cannot resolve, cannot
    // be paired with whatever the gateway gathered.
    info!(candidates = %describe_candidates(&offer_sdp), "the browser's offer");
    peer.set_remote_description(offer).await?;
    let answer = peer.create_answer(None).await?;
    peer.set_local_description(answer).await?;

    // A deadline, not a cliff: a session with a path to offer is answered late
    // rather than refused, and one that has everything it could gather does not
    // wait for sockets that can only repeat it (relaysight-vms-0kem).
    let gathered = tokio::time::timeout(tuning.gathering_budget, async {
        tokio::select! {
            () = gather_complete.notified() => Gathered::Complete,
            () = gathering.enough.notified() => Gathered::Enough,
        }
    })
    .await
    .unwrap_or(Gathered::OutOfTime);
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
    if !sdp.contains("a=candidate:") {
        anyhow::bail!(no_path_refusal(&urls, tuning.gathering_budget));
    }
    // Gathering may well still be running when it is cut short, and nothing it
    // produces from here would ever reach the browser, so the answer says so.
    let sdp = match gathered {
        Gathered::Complete => sdp,
        Gathered::Enough | Gathered::OutOfTime => declare_the_candidates_final(&sdp),
    };
    // The other half of "the browser's offer" above: what this end brought, and
    // how the gathering that produced it ended. A session that fails to connect
    // is diagnosed from the pair of them.
    info!(
        candidates = %describe_candidates(&sdp),
        gathering = ?gathered,
        "the gateway's answer"
    );
    if gathered == Gathered::OutOfTime {
        warn!(
            budget_seconds = tuning.gathering_budget.as_secs(),
            missing = ?gathering.missing(),
            "gathering ran out of time; answered with the candidates in hand"
        );
    }
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
            let (path, candidate_url) = crate::icepath::observed_candidate(&peer_stats).await;
            // Which way a relayed session reaches the relay: plain UDP, or a TLS/TCP bridge.
            let relay_transport = candidate_url
                .filter(|_| path.is_relayed())
                .and_then(|url| bridges.transport_for_url(&url))
                .map_or_else(|| "-".to_owned(), |transport| transport.to_string());
            info!(
                session_id = %task_session_id,
                path = %path,
                relayed = path.is_relayed(),
                relay_transport = %relay_transport,
                "WebRTC peer connected; starting RTSP forwarding"
            );
            let mut source = media.open().await?;
            pump_from(
                source.as_mut(),
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
        // What each bridge carried: the per-session relay byte count the cost model wants.
        for stats in bridges.stats() {
            info!(
                session_id = %task_session_id,
                bytes_up = stats.bytes_up(),
                bytes_down = stats.bytes_down(),
                "TURN bridge closed"
            );
        }
        // The bridges outlive the peer connection by exactly this long.
        drop(bridges);
    });

    Ok((
        LiveSessionAnswer {
            session_id,
            sdp,
            sdp_type,
            codec: "H264".into(),
            expires_at,
        },
        bridge_stats,
    ))
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

/// Frames onto a WebRTC track, from whatever produced them.
///
/// A sample's duration is only known once the next frame arrives, so one frame
/// is always held back; the last one is dropped when the session ends, which is
/// what the browser would have discarded anyway.
pub(crate) async fn pump_from(
    source: &mut dyn crate::frames::FrameSource,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    done: Arc<Notify>,
    max_duration: Duration,
) -> anyhow::Result<()> {
    let payload_type = negotiated_payload_type(&sender).await?;
    let track_ssrc = *track
        .ssrcs()
        .await
        .first()
        .ok_or_else(|| anyhow!("WebRTC track has no SSRC"))?;
    let deadline = tokio::time::sleep(max_duration);
    tokio::pin!(deadline);
    let mut started = false;
    // A frame waits here until the next one's timestamp says how long it lasted, so
    // the instant it arrived travels with it: that is what stamps the packets, and
    // it is the frame's own time rather than the moment this loop reached it.
    let mut pending: Option<(Bytes, i64, u32, Instant)> = None;

    loop {
        tokio::select! {
            _ = done.notified() => break,
            _ = &mut deadline => break,
            frame = source.next_frame() => match frame? {
                None => break,
                Some(frame) => {
                    if !started {
                        if !frame.keyframe { continue; }
                        started = true;
                    }
                    let timestamp = frame.timestamp;
                    if let Some((data, previous_timestamp, clock_rate, arrived)) =
                        pending.replace((frame.data, timestamp, frame.clock_rate, Instant::now()))
                    {
                        let ticks = timestamp.saturating_sub(previous_timestamp);
                        let duration = crate::rtsp::frame_duration(ticks, clock_rate);
                        track.sample_writer(track_ssrc, payload_type).write_sample(&Sample {
                            data, duration, ..Sample::new(arrived)
                        }).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The ICE candidates in an offer, counted by kind, with mDNS names called out
/// separately: `2 host (2 mdns), 1 srflx`. Empty means the browser gathered
/// nothing, which is its own diagnosis.
fn describe_candidates(sdp: &str) -> String {
    let mut counts: Vec<(String, usize, usize)> = Vec::new();
    for line in sdp.lines() {
        let Some(candidate) = line.trim().strip_prefix("a=candidate:") else {
            continue;
        };
        let fields: Vec<&str> = candidate.split(' ').collect();
        let Some(kind) = fields
            .iter()
            .position(|f| *f == "typ")
            .and_then(|at| fields.get(at + 1))
        else {
            continue;
        };
        let is_mdns = fields
            .get(4)
            .is_some_and(|address| address.ends_with(".local"));
        match counts.iter_mut().find(|(seen, _, _)| seen == kind) {
            Some((_, total, mdns)) => {
                *total += 1;
                *mdns += usize::from(is_mdns);
            }
            None => counts.push(((*kind).to_owned(), 1, usize::from(is_mdns))),
        }
    }
    if counts.is_empty() {
        return "none".to_owned();
    }
    counts
        .into_iter()
        .map(|(kind, total, mdns)| {
            if mdns > 0 {
                format!("{total} {kind} ({mdns} mdns)")
            } else {
                format!("{total} {kind}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuse before negotiating, with a reason an operator can act on.
///
/// A browser with no H.264 at all — Firefox whose OpenH264 plugin never
/// downloaded is the common one — cannot receive this stream, and neither can
/// one that only offers single-NAL mode. Saying so beats the library's
/// "payload type not found".
fn refuse_unless_the_browser_can_receive_h264(offer_sdp: &str) -> anyhow::Result<()> {
    let h264_payload_types: Vec<&str> = offer_sdp
        .lines()
        .filter(|line| line.contains("H264/90000"))
        .filter_map(|line| line.trim().strip_prefix("a=rtpmap:")?.split(' ').next())
        .collect();
    if h264_payload_types.is_empty() {
        anyhow::bail!(
            "this browser cannot receive H.264, and the stream is passed through as H.264 \
             rather than transcoded. Chrome, Edge and Safari have it. Firefox needs OpenH264, \
             which recent versions ship but leave switched off: set \
             media.gmp-gmpopenh264.enabled to true in about:config, and check about:support \
             under Media that the plugin is actually installed"
        );
    }
    let mode_one = h264_payload_types.iter().any(|payload_type| {
        offer_sdp.lines().any(|line| {
            line.trim()
                .strip_prefix(&format!("a=fmtp:{payload_type} "))
                .is_some_and(|parameters| parameters.contains("packetization-mode=1"))
        })
    });
    if !mode_one {
        anyhow::bail!(
            "this browser offered H.264 in single-NAL mode only (packetization-mode=0), and \
             this gateway sends fragmented H.264, which it would not be able to decode"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rtc::stun::{
        message::{BINDING_SUCCESS, Message},
        xoraddr::XorMappedAddress,
    };

    use super::LiveMedia;

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
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
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
    async fn live_works_whichever_way_the_camera_supplies_parameter_sets() {
        // Passthrough sends the camera's H.264 to the browser untouched, so if the
        // parameter sets never reach the wire the browser has a stream it cannot
        // decode. Both vendor dialects have to end in media arriving.
        for mode in [
            crate::fake_camera::ParameterSets::SdpOnly,
            crate::fake_camera::ParameterSets::InBandOnly,
        ] {
            let camera = FakeCamera::start_with(false, mode).await.unwrap();
            let browser = FakeBrowser::offer().await.unwrap();
            let answer = super::start_h264(
                LiveMedia::Rtsp {
                    url: camera.url.clone(),
                    username: None,
                    password: None,
                },
                browser.offer_sdp().to_owned(),
                "offer".into(),
                Vec::new(),
                10,
            )
            .await
            .unwrap_or_else(|e| panic!("{mode:?}: gateway did not answer: {e:#}"));

            browser.accept_answer(&answer.sdp).await.unwrap();
            let received = browser
                .wait_for_media(Duration::from_secs(15))
                .await
                .unwrap_or_else(|e| panic!("{mode:?}: no media reached the browser: {e:#}"));
            assert!(
                received.payload_bytes > 1000,
                "{mode:?}: only {} payload bytes, which is padding rather than video",
                received.payload_bytes
            );
        }
    }

    /// An offer from Chrome, not from the fake browser in this repository.
    ///
    /// `FakeBrowser` is webrtc-rs offering what webrtc-rs registers, so it
    /// agrees with this code by construction. A real browser offers a dozen
    /// codecs, several H.264 profiles at payload types of its own choosing,
    /// and rtx and fec beside them. The fixture is the SDP Chrome produced for
    /// `addTransceiver('video', {direction: 'recvonly'})`, which is exactly
    /// what the dashboard sends.
    #[tokio::test]
    async fn a_real_browsers_offer_is_answered_with_h264() {
        let answer = answer_for(CHROME_OFFER.to_owned())
            .await
            .unwrap_or_else(|e| panic!("a real browser's offer was not answered: {e:#}"));
        assert_offers_payload_type(CHROME_OFFER, &answer);
        // The browser needs somewhere to send to: an answer with no candidates
        // is a session that cannot start, and the browser has no second chance
        // because this signalling is not trickle.
        assert!(
            answer
                .sdp
                .lines()
                .any(|line| line.starts_with("a=candidate")),
            "the answer carries no ICE candidates:\n{}",
            answer.sdp
        );
    }

    /// The same browser, with only the H.264 profiles this gateway does not
    /// itself register left in the offer.
    ///
    /// Chrome offers 42001f, 42e01f, 4d001f and f4001f; Firefox and Safari
    /// pick their own subsets, and a browser is free to offer none of the ones
    /// a gateway happens to have registered. Registering a single profile and
    /// hoping means WebRTC fails with "RTPSender created with no codecs",
    /// which is what this looked like from the dashboard.
    #[tokio::test]
    async fn a_browser_offering_another_h264_profile_is_still_answered() {
        let narrowed = without_payload_types(CHROME_OFFER, |line| {
            line.contains("profile-level-id=42e01f")
        });
        assert!(
            narrowed.contains("H264/90000"),
            "the narrowed offer must still offer H.264"
        );
        let answer = answer_for(narrowed.clone())
            .await
            .unwrap_or_else(|e| panic!("another H.264 profile was refused: {e:#}"));
        assert_offers_payload_type(&narrowed, &answer);
    }

    /// A browser that cannot receive H.264 at all — Firefox whose OpenH264
    /// plugin never downloaded looks like this — must be told what is missing.
    /// Without the check it reaches WebRTC's own "RTPSender created with no
    /// codecs", which says nothing about H.264, about the browser, or about
    /// what to do next.
    #[tokio::test]
    async fn a_browser_without_h264_is_told_what_is_missing() {
        let without_h264 =
            without_payload_types(CHROME_OFFER, |line| line.contains("profile-level-id="));
        assert!(
            !without_h264.contains("H264/90000"),
            "the narrowed offer still has H.264 in it"
        );
        let error = answer_for(without_h264)
            .await
            .expect_err("a browser that cannot receive H.264 was answered anyway");
        let said = format!("{error:#}");
        assert!(
            said.contains("H.264") && said.contains("OpenH264"),
            "the refusal must name what is missing and where to look: {said}"
        );
    }

    const CHROME_OFFER: &str = include_str!("../tests/fixtures/chrome-recvonly-offer.sdp");

    async fn answer_for(offer: String) -> anyhow::Result<super::LiveSessionAnswer> {
        let camera = FakeCamera::start(false).await.unwrap();
        super::start_h264(
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
            offer,
            "offer".into(),
            Vec::new(),
            10,
        )
        .await
    }

    /// The same offer with a set of payload types taken out of it altogether:
    /// the `m=` line, the `a=rtpmap`, `a=fmtp` and `a=rtcp-fb` lines, and any
    /// rtx payload type that pointed at them with `apt=`.
    ///
    /// Leaving a number in the `m=` line without its `a=rtpmap` produces an
    /// SDP no browser would send, and the library rejects it out of hand with
    /// "payload type not found" — which is not the failure being studied here.
    fn without_payload_types(offer: &str, doomed: impl Fn(&str) -> bool) -> String {
        let mut gone: Vec<String> = offer
            .lines()
            .filter(|line| line.starts_with("a=fmtp:") && doomed(line))
            .filter_map(|line| {
                line.trim_start_matches("a=fmtp:")
                    .split(' ')
                    .next()
                    .map(str::to_owned)
            })
            .collect();
        // The rtx payload types that exist only to repair these.
        for line in offer.lines() {
            if let Some(parameters) = line.strip_prefix("a=fmtp:") {
                let (payload_type, rest) = parameters.split_once(' ').unwrap_or((parameters, ""));
                if let Some(apt) = rest.strip_prefix("apt=")
                    && gone.iter().any(|pt| pt == apt)
                {
                    gone.push(payload_type.to_owned());
                }
            }
        }

        offer
            .lines()
            .filter(|line| {
                !gone.iter().any(|pt| {
                    line.starts_with(&format!("a=rtpmap:{pt} "))
                        || line.starts_with(&format!("a=fmtp:{pt} "))
                        || line.starts_with(&format!("a=rtcp-fb:{pt} "))
                })
            })
            .map(|line| {
                if let Some(formats) = line.strip_prefix("m=video ") {
                    let mut parts: Vec<&str> = formats.split(' ').collect();
                    parts.retain(|part| !gone.iter().any(|pt| pt == part));
                    format!("m=video {}\r\n", parts.join(" "))
                } else {
                    format!("{line}\r\n")
                }
            })
            .collect()
    }

    /// An answer has to name a payload type the browser itself offered for
    /// H.264, or the browser will not recognise what arrives.
    fn assert_offers_payload_type(offer: &str, answer: &super::LiveSessionAnswer) {
        let video = answer
            .sdp
            .split("m=")
            .find(|section| section.starts_with("video"))
            .expect("an answer with video");
        let line = video
            .lines()
            .find(|line| line.contains("H264/90000"))
            .expect("H.264 in the answer");
        let payload_type = line
            .trim_start_matches("a=rtpmap:")
            .split(' ')
            .next()
            .expect("a payload type");
        assert!(
            offer.contains(&format!("a=rtpmap:{payload_type} H264/90000")),
            "the answer chose payload type {payload_type}, which the browser never offered"
        );
    }

    #[tokio::test]
    async fn an_answer_is_refused_as_an_offer() {
        // The command carries the SDP type from the browser. Treating an answer
        // as an offer would produce an unusable session rather than an error.
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let error = super::start_h264(
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
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
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
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

    #[test]
    fn what_gathering_waits_for_follows_the_servers_the_session_was_given() {
        let expect = |urls: &[&str]| {
            let urls: Vec<String> = urls.iter().map(|url| (*url).to_owned()).collect();
            super::kinds_to_expect(&urls, None)
        };
        assert!(
            expect(&[]).nothing(),
            "with no servers there is nothing to wait for: the host's own addresses \
             are gathered before anything is asked"
        );
        assert_eq!(
            expect(&["stun:stun.example:3478"]),
            super::ServerKinds {
                srflx: true,
                relay: false
            }
        );
        assert_eq!(
            expect(&["turn:relay.example:3478"]),
            super::ServerKinds {
                srflx: false,
                relay: true
            },
            "this library asks stun: URLs only for a reflexive candidate, so a \
             relay-only configuration must not sit waiting for one"
        );
        assert_eq!(
            expect(&["stun:stun.example:3478", "turns:relay.example:443"]),
            super::ServerKinds {
                srflx: true,
                relay: true
            }
        );
        assert_eq!(
            super::kinds_to_expect(
                &[
                    "stun:stun.example:3478".to_owned(),
                    "turn:relay.example:3478".to_owned()
                ],
                Some(rtc::peer_connection::configuration::RTCIceTransportPolicy::Relay)
            ),
            super::ServerKinds {
                srflx: false,
                relay: true
            },
            "a session pinned to the relay gathers nothing else, so a reflexive \
             candidate is not owed however many STUN servers it was given"
        );
    }

    #[test]
    fn a_session_with_no_path_at_all_names_the_servers_that_never_answered() {
        let refusal = super::no_path_refusal(
            &["stun:stun.l.google.com:19302".to_owned()],
            Duration::from_secs(12),
        );
        assert!(
            refusal.contains("12s") && refusal.contains("stun:stun.l.google.com:19302"),
            "an operator cannot act on a refusal that names neither the wait nor the \
             server: {refusal}"
        );
    }

    #[test]
    fn an_answer_says_when_no_further_candidate_is_coming() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
                   a=candidate:1 1 udp 1 10.0.0.1 5 typ host\r\n";
        let declared = super::declare_the_candidates_final(sdp);
        assert!(declared.ends_with("a=end-of-candidates\r\n"), "{declared}");
        assert_eq!(
            super::declare_the_candidates_final(&declared),
            declared,
            "an answer that already says so must not say it twice"
        );
    }

    #[test]
    fn the_interfaces_the_gateway_offers_for_media_can_be_pinned() {
        assert_eq!(
            super::media_udp_addrs_from(None),
            vec!["0.0.0.0:0".to_owned()],
            "unset means every interface, which is what it meant before the setting existed"
        );
        assert_eq!(
            super::media_udp_addrs_from(Some("  ")),
            vec!["0.0.0.0:0".to_owned()],
            "an empty setting cannot mean binding nothing: that is a gateway with no live view"
        );
        assert_eq!(
            super::media_udp_addrs_from(Some("192.168.1.10")),
            vec!["192.168.1.10:0".to_owned()],
            "an interface named without a port means any port on it"
        );
        assert_eq!(
            super::media_udp_addrs_from(Some(" 192.168.1.10:40000 , 10.8.0.2 ")),
            vec!["192.168.1.10:40000".to_owned(), "10.8.0.2:0".to_owned()]
        );
    }

    /// A STUN server on loopback that answers a binding request with the
    /// sender's own address, so a session gathers a reflexive candidate from it.
    async fn an_answering_stun_server() -> String {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let url = format!("stun:{}", socket.local_addr().unwrap());
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 1500];
            while let Ok((len, from)) = socket.recv_from(&mut buffer).await {
                let mut request = Message::new();
                if request.unmarshal_binary(&buffer[..len]).is_err() {
                    continue;
                }
                let mut response = Message::new();
                response
                    .build(&[
                        Box::new(BINDING_SUCCESS),
                        Box::new(request.transaction_id),
                        Box::new(XorMappedAddress {
                            ip: from.ip(),
                            port: from.port(),
                        }),
                    ])
                    .unwrap();
                let _ = socket.send_to(&response.raw, from).await;
            }
        });
        url
    }

    /// A STUN server that is bound and never answers: the Docker bridge of
    /// relaysight-vms-0kem, reduced to something a test can rely on. The port
    /// stays open for the life of the test on purpose — a closed one is refused
    /// rather than ignored, which is a different thing to wait for.
    async fn a_silent_stun_server() -> String {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let url = format!("stun:{}", socket.local_addr().unwrap());
        tokio::spawn(async move {
            let _socket = socket;
            std::future::pending::<()>().await;
        });
        url
    }

    fn stun_servers(urls: Vec<String>) -> Vec<vms_domain::RtcIceServerConfig> {
        vec![vms_domain::RtcIceServerConfig {
            urls,
            username: String::new(),
            credential: String::new(),
        }]
    }

    #[tokio::test]
    async fn a_stun_server_that_never_answers_does_not_hold_up_a_session() {
        // The shape that took live view down on this machine on 2026-10-20
        // (relaysight-vms-0kem): the gateway binds one UDP socket per interface
        // and sends a STUN binding request from each, and a host with Docker
        // bridges has several that can reach nothing. webrtc-rs abandons a
        // silent server only after 7.8 seconds, which was most of the gathering
        // budget — spent with host candidates ready in the first millisecond
        // and a reflexive one from the server that did answer.
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let ice_servers = stun_servers(vec![
            an_answering_stun_server().await,
            a_silent_stun_server().await,
        ]);

        let started = std::time::Instant::now();
        let (answer, _) = super::start_h264_with(
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
            browser.offer_sdp().to_owned(),
            "offer".into(),
            ice_servers,
            10,
            super::LiveTuning {
                udp_addrs: vec!["127.0.0.1:0".to_owned()],
                ..Default::default()
            },
        )
        .await
        .expect("the gateway answers the offer");
        let took = started.elapsed();

        assert!(
            answer.sdp.contains("typ srflx"),
            "nothing was gathered from the server that answers:\n{}",
            answer.sdp
        );
        assert!(
            took < Duration::from_secs(3),
            "answering took {took:?}, so the session waited for a server that never answers"
        );

        browser.accept_answer(&answer.sdp).await.unwrap();
        let received = browser
            .wait_for_media(Duration::from_secs(15))
            .await
            .expect("media must arrive");
        assert!(
            received.payload_bytes > 1000,
            "only {} payload bytes arrived, which is padding rather than video",
            received.payload_bytes
        );
    }

    #[tokio::test]
    async fn gathering_that_runs_out_of_time_answers_with_the_candidates_in_hand() {
        // The budget used to be a cliff: everything gathered was thrown away and
        // the session refused, which is the worst of both — the operator waits,
        // and then gets nothing, over a path that would have carried the video.
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let ice_servers = stun_servers(vec![a_silent_stun_server().await]);

        let (answer, _) = super::start_h264_with(
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
            browser.offer_sdp().to_owned(),
            "offer".into(),
            ice_servers,
            10,
            super::LiveTuning {
                udp_addrs: vec!["127.0.0.1:0".to_owned()],
                gathering_budget: Duration::from_millis(200),
                ..Default::default()
            },
        )
        .await
        .expect("a session holding a usable candidate is answered, not refused");

        assert!(
            answer.sdp.contains("typ host"),
            "the candidates gathered before the deadline are missing from the answer:\n{}",
            answer.sdp
        );
        assert!(
            answer.sdp.contains("a=end-of-candidates"),
            "this answer is the only message the session sends, so it has to say \
             that these candidates are all there will be:\n{}",
            answer.sdp
        );

        browser.accept_answer(&answer.sdp).await.unwrap();
        let received = browser
            .wait_for_media(Duration::from_secs(15))
            .await
            .expect("media must arrive over the candidate that was gathered in time");
        assert!(
            received.payload_bytes > 1000,
            "only {} payload bytes arrived, which is padding rather than video",
            received.payload_bytes
        );
    }

    #[tokio::test]
    #[ignore = "needs a local TLS relay; run make check-gateway-relay"]
    async fn a_session_relays_over_tls_when_udp_to_the_relay_is_blocked() {
        let var = |name: &str| {
            std::env::var(name)
                .unwrap_or_else(|_| panic!("set {name}; run make check-gateway-relay"))
        };
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let ice_servers = vec![vms_domain::RtcIceServerConfig {
            urls: vec![
                var("RELAYSIGHT_TEST_TURN_UDP_URL"),
                var("RELAYSIGHT_TEST_TURNS_URL"),
            ],
            username: var("RELAYSIGHT_TEST_TURN_USER"),
            credential: var("RELAYSIGHT_TEST_TURN_PASS"),
        }];

        let (answer, bridges) = super::start_h264_with(
            LiveMedia::Rtsp {
                url: camera.url.clone(),
                username: None,
                password: None,
            },
            browser.offer_sdp().to_owned(),
            "offer".into(),
            ice_servers,
            20,
            super::LiveTuning {
                ice_transport_policy: Some(
                    rtc::peer_connection::configuration::RTCIceTransportPolicy::Relay,
                ),
                ..Default::default()
            },
        )
        .await
        .expect("the gateway answers with a relayed candidate");
        assert_eq!(
            bridges.len(),
            1,
            "UDP to the relay is blocked, so exactly one TLS bridge must carry the session"
        );

        browser.accept_answer(&answer.sdp).await.unwrap();
        let received = browser
            .wait_for_media(Duration::from_secs(20))
            .await
            .expect("media must arrive through the relay");
        assert!(
            received.payload_bytes > 1000,
            "only {} payload bytes arrived",
            received.payload_bytes
        );
        // Limited to relay candidates, the only way out is the bridge, so the
        // video has to show up in its counter.
        assert!(
            bridges[0].bytes_up() > received.payload_bytes,
            "{} bytes crossed the TLS bridge for {} payload bytes received",
            bridges[0].bytes_up(),
            received.payload_bytes
        );
    }
}
