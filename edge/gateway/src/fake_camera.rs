//! A minimal RTSP camera for tests.
//!
//! The media path could only ever be tested where its logic was pure — profile
//! selection, credential handling, frame timing. Everything that talks RTSP was
//! unreachable without hardware, which is precisely where a passthrough pipeline
//! goes wrong. This serves a real H.264 stream (`fixtures/camera.h264`, produced
//! once with ffmpeg and committed, so the build needs no encoder) over RTSP with
//! RTP interleaved on the TCP control connection — the transport discovery asks
//! cameras for.
//!
//! It is deliberately not a conformant server. It answers the five methods the
//! gateway sends and nothing else.

use std::net::SocketAddr;

use anyhow::{Context, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const CLOCK_RATE: u32 = 90_000;
const TICKS_PER_FRAME: u32 = CLOCK_RATE / 30;
const PAYLOAD_TYPE: u8 = 96;
/// Below the usual 1500-byte MTU with room for RTP and interleave headers.
const MAX_PAYLOAD: usize = 1400;

/// Where a camera puts its H.264 parameter sets.
///
/// Vendors differ and both of these are common in the field. A decoder that quietly
/// depends on one of them works against half the cameras and fails against the rest,
/// which is the compatibility tail this project has to grind through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterSets {
    /// SDP `sprop-parameter-sets` and repeated in-band before each IDR.
    Both,
    /// SDP only. Nothing in the stream, so a decoder that ignores the SDP never
    /// learns the dimensions and cannot start.
    SdpOnly,
    /// In-band only, no `sprop-parameter-sets` in the SDP. A decoder that reads only
    /// the SDP waits forever.
    InBandOnly,
}

/// One access unit: the VCL NAL plus any parameter sets that preceded it.
struct AccessUnit {
    nals: Vec<Vec<u8>>,
    keyframe: bool,
}

/// Split an Annex-B stream into access units. A VCL NAL (type 1 or 5) closes the
/// unit; SPS, PPS and SEI accumulate onto the next one.
fn parse_annex_b(data: &[u8]) -> Vec<AccessUnit> {
    let mut nals: Vec<Vec<u8>> = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (idx, &begin) in starts.iter().enumerate() {
        let mut end = starts.get(idx + 1).map_or(data.len(), |next| next - 3);
        // A four-byte start code leaves a trailing zero on the previous NAL.
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        nals.push(data[begin..end].to_vec());
    }

    let mut units = Vec::new();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    for nal in nals {
        let kind = nal[0] & 0x1f;
        pending.push(nal);
        if kind == 1 || kind == 5 {
            units.push(AccessUnit {
                keyframe: kind == 5,
                nals: std::mem::take(&mut pending),
            });
        }
    }
    units
}

fn extract_parameter_sets(units: &[AccessUnit]) -> (Vec<u8>, Vec<u8>) {
    let find = |kind: u8| {
        units
            .iter()
            .flat_map(|u| u.nals.iter())
            .find(|n| n[0] & 0x1f == kind)
            .cloned()
            .unwrap_or_default()
    };
    (find(7), find(8))
}

fn rtp_header(marker: bool, sequence: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = if marker {
        0x80 | PAYLOAD_TYPE
    } else {
        PAYLOAD_TYPE
    };
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// Frame one RTP packet for the interleaved channel: `$`, channel, 16-bit length.
fn interleave(channel: u8, packet: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packet.len() + 4);
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    out.extend_from_slice(packet);
    out
}

/// Packetise one NAL per RFC 6184: whole when it fits, FU-A fragments when not.
fn packetise(
    nal: &[u8],
    last_in_unit: bool,
    sequence: &mut u16,
    timestamp: u32,
    ssrc: u32,
) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    if nal.len() <= MAX_PAYLOAD {
        let mut p = rtp_header(last_in_unit, *sequence, timestamp, ssrc).to_vec();
        *sequence = sequence.wrapping_add(1);
        p.extend_from_slice(nal);
        packets.push(p);
        return packets;
    }

    let header = nal[0];
    let indicator = (header & 0xe0) | 28;
    let kind = header & 0x1f;
    let body = &nal[1..];
    let chunks: Vec<&[u8]> = body.chunks(MAX_PAYLOAD - 2).collect();
    let count = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let start = idx == 0;
        let end = idx + 1 == count;
        let mut p = rtp_header(last_in_unit && end, *sequence, timestamp, ssrc).to_vec();
        *sequence = sequence.wrapping_add(1);
        p.push(indicator);
        p.push((u8::from(start) << 7) | (u8::from(end) << 6) | kind);
        p.extend_from_slice(chunk);
        packets.push(p);
    }
    packets
}

pub struct FakeCamera {
    pub url: String,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeCamera {
    /// Start on an ephemeral loopback port and serve one session.
    ///
    /// `require_credentials` makes every request answer 401 unless an
    /// Authorization header is present, which is how the credential plumbing gets
    /// exercised end to end rather than only in unit tests.
    pub async fn start(require_credentials: bool) -> anyhow::Result<Self> {
        Self::start_with(require_credentials, ParameterSets::Both).await
    }

    /// As `start`, but choosing where the parameter sets come from.
    pub async fn start_with(
        require_credentials: bool,
        parameter_sets: ParameterSets,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind fake camera")?;
        let addr: SocketAddr = listener.local_addr()?;
        let url = format!("rtsp://{addr}/stream");
        let task = tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await
                && let Err(error) = serve(socket, require_credentials, parameter_sets).await
            {
                eprintln!("fake camera session ended: {error:#}");
            }
        });
        Ok(Self { url, _task: task })
    }
}

async fn serve(
    mut socket: TcpStream,
    require_credentials: bool,
    parameter_sets: ParameterSets,
) -> anyhow::Result<()> {
    let raw = include_bytes!("../fixtures/camera.h264");
    let units = parse_annex_b(raw);
    let (sps, pps) = extract_parameter_sets(&units);

    let mut buffer = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        let read = socket.read(&mut scratch).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&scratch[..read]);

        while let Some(end) = find_request_end(&buffer) {
            let request = String::from_utf8_lossy(&buffer[..end]).to_string();
            buffer.drain(..end);
            let cseq = header_value(&request, "CSeq").unwrap_or_else(|| "0".into());
            let method = request.split_whitespace().next().unwrap_or("").to_owned();
            let authorised =
                !require_credentials || header_value(&request, "Authorization").is_some();

            if !authorised {
                let body = format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: Basic realm=\"fake\"\r\n\r\n"
                );
                socket.write_all(body.as_bytes()).await?;
                continue;
            }

            match method.as_str() {
                "OPTIONS" => {
                    let body = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n"
                    );
                    socket.write_all(body.as_bytes()).await?;
                }
                "DESCRIBE" => {
                    let sdp = format!(
                        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=fake\r\nt=0 0\r\n\
                         m=video 0 RTP/AVP {PAYLOAD_TYPE}\r\nc=IN IP4 0.0.0.0\r\n\
                         a=rtpmap:{PAYLOAD_TYPE} H264/{CLOCK_RATE}\r\n\
                         a=fmtp:{PAYLOAD_TYPE} packetization-mode=1{}\r\n\
                         a=control:streamid=0\r\n",
                        match parameter_sets {
                            ParameterSets::InBandOnly => String::new(),
                            _ => format!(
                                ";sprop-parameter-sets={},{}",
                                BASE64.encode(&sps),
                                BASE64.encode(&pps)
                            ),
                        },
                    );
                    let body = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                        sdp.len()
                    );
                    socket.write_all(body.as_bytes()).await?;
                }
                "SETUP" => {
                    let body = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                    );
                    socket.write_all(body.as_bytes()).await?;
                }
                "PLAY" => {
                    let body =
                        format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678\r\n\r\n");
                    socket.write_all(body.as_bytes()).await?;
                    stream_units(&mut socket, &units, parameter_sets).await?;
                    return Ok(());
                }
                "TEARDOWN" => {
                    let body = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n");
                    socket.write_all(body.as_bytes()).await?;
                    return Ok(());
                }
                other => return Err(anyhow!("fake camera got unexpected method {other}")),
            }
        }
    }
}

async fn stream_units(
    socket: &mut TcpStream,
    units: &[AccessUnit],
    parameter_sets: ParameterSets,
) -> anyhow::Result<()> {
    let ssrc = 0x1234_5678;
    let mut sequence: u16 = 0;
    let mut timestamp: u32 = 0;
    for unit in units {
        // A camera that advertises its parameter sets in the SDP often does not
        // repeat them in the stream. Drop them here to reproduce that.
        let nals: Vec<&Vec<u8>> = unit
            .nals
            .iter()
            .filter(|n| parameter_sets != ParameterSets::SdpOnly || !matches!(n[0] & 0x1f, 7 | 8))
            .collect();
        let count = nals.len();
        for (idx, nal) in nals.iter().enumerate() {
            let last = idx + 1 == count;
            for packet in packetise(nal, last, &mut sequence, timestamp, ssrc) {
                socket.write_all(&interleave(0, &packet)).await?;
            }
        }
        timestamp = timestamp.wrapping_add(TICKS_PER_FRAME);
        // Real time between frames, so anything reading this sees a live stream
        // rather than one burst.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    Ok(())
}

fn find_request_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn header_value(request: &str, name: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::{AccessUnit, extract_parameter_sets, packetise, parse_annex_b};

    fn fixture() -> Vec<AccessUnit> {
        parse_annex_b(include_bytes!("../fixtures/camera.h264"))
    }

    #[test]
    fn the_fixture_has_the_shape_the_tests_rely_on() {
        let units = fixture();
        let keyframes = units.iter().filter(|u| u.keyframe).count();
        assert!(
            units.len() > 20,
            "expected a real stream, got {}",
            units.len()
        );
        assert!(
            keyframes >= 2,
            "two keyframes are needed to exercise segmentation, found {keyframes}"
        );
        assert!(units[0].keyframe, "the stream must open on a keyframe");
    }

    #[test]
    fn parameter_sets_are_present_and_well_formed() {
        let units = fixture();
        let (sps, pps) = extract_parameter_sets(&units);
        assert_eq!(sps[0] & 0x1f, 7);
        assert_eq!(pps[0] & 0x1f, 8);
    }

    #[test]
    fn a_large_nal_is_fragmented_and_reassembles_to_the_original() {
        let mut nal = vec![0x65];
        nal.extend((0..5000u32).map(|i| (i % 251) as u8));
        let mut sequence = 0;
        let packets = packetise(&nal, true, &mut sequence, 0, 1);
        assert!(packets.len() > 1, "a 5 KB NAL must fragment");

        let mut rebuilt = vec![(packets[0][13] & 0x1f) | (packets[0][12] & 0xe0)];
        for packet in &packets {
            rebuilt.extend_from_slice(&packet[14..]);
        }
        assert_eq!(rebuilt, nal, "FU-A fragments must reassemble exactly");

        // Only the final fragment carries the marker bit.
        assert_eq!(packets.last().unwrap()[1] & 0x80, 0x80);
        assert_eq!(packets[0][1] & 0x80, 0);
    }

    #[test]
    fn a_small_nal_travels_whole() {
        let nal = vec![0x68, 0xee, 0x3c, 0x80];
        let mut sequence = 0;
        let packets = packetise(&nal, false, &mut sequence, 0, 1);
        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][12..], &nal[..]);
    }
}
