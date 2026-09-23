//! Video pushed over SRT, which is what an encoder on a lossy link speaks.
//!
//! The stream id names the source — `SRT_LISTEN=0.0.0.0:9000` and a publisher
//! calling with stream id `loading-bay` — and only keys the dashboard listed
//! are accepted, exactly as RTMP works. SRT carries MPEG-TS, so the H.264 comes
//! out of a transport stream rather than a container that describes itself:
//! `ts` does that, and the parameter sets arrive in band.
//!
//! Behind the `srt` feature until this has run against a real encoder. What it
//! has run against is an SRT caller in this process and a transport stream
//! ffmpeg produced.

pub mod ts;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};

use futures::StreamExt;
use srt_tokio::{SrtListener, access::*};

use crate::{
    frames::Frame,
    h264,
    ingest::{Ingest, StreamState, lock},
};

/// MPEG-TS counts in 90 kHz, and so does the stream's PTS.
const TS_CLOCK_RATE: u32 = 90_000;

/// Where the listener binds, if SRT ingest is on at all.
pub fn listen_address() -> Option<SocketAddr> {
    let raw = std::env::var("SRT_LISTEN").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse() {
        Ok(address) => Some(address),
        Err(error) => {
            tracing::warn!(%error, value = %raw, "SRT_LISTEN is not an address; SRT ingest is off");
            None
        }
    }
}

/// Accept publishers until the process ends.
pub async fn serve(address: SocketAddr, ingest: Ingest) -> anyhow::Result<()> {
    let (_listener, mut incoming) = SrtListener::builder().bind(address).await?;
    while let Some(request) = incoming.incoming().next().await {
        let key = request
            .stream_id()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_default();
        let key = stream_key(&key);
        if key.is_empty() || !ingest.is_allowed(&key) {
            tracing::warn!(key = %key, "refused an SRT publisher: no such source");
            let _ = request
                .reject(RejectReason::Server(ServerRejectReason::Forbidden))
                .await;
            continue;
        }
        let ingest = ingest.clone();
        tokio::spawn(async move {
            let socket = match request.accept(None).await {
                Ok(socket) => socket,
                Err(error) => {
                    tracing::warn!(key = %key, %error, "SRT publisher did not connect");
                    return;
                }
            };
            tracing::info!(key = %key, "an SRT publisher started");
            let state = ingest.open(&key);
            if let Err(error) = carry(socket, &ingest, &key, &state).await {
                tracing::info!(key = %key, %error, "SRT stream ended");
            }
            // Consumers learn the publisher left the same way they do on RTMP.
            ingest.close(&key);
        });
    }
    Ok(())
}

/// An encoder may send `#!::r=<key>,m=publish`, the convention SRT uses for
/// naming a resource, or just the key.
fn stream_key(stream_id: &str) -> String {
    let Ok(list) = stream_id.parse::<AccessControlList>() else {
        return stream_id.trim().to_owned();
    };
    for entry in list.0 {
        if let Ok(StandardAccessControlEntry::ResourceName(name)) =
            StandardAccessControlEntry::try_from(entry)
        {
            return name;
        }
    }
    stream_id.trim().to_owned()
}

async fn carry(
    mut socket: srt_tokio::SrtSocket,
    ingest: &Ingest,
    key: &str,
    state: &Arc<Mutex<StreamState>>,
) -> anyhow::Result<()> {
    let mut demuxer = ts::Demuxer::new();
    while let Some(packet) = socket.next().await {
        let (_, bytes) = packet?;
        for unit in demuxer.push(&bytes)? {
            publish(unit, ingest, key, state);
        }
    }
    for unit in demuxer.flush() {
        publish(unit, ingest, key, state);
    }
    Ok(())
}

/// One access unit, in the framing everything downstream already reads.
fn publish(unit: ts::AccessUnit, ingest: &Ingest, key: &str, state: &Arc<Mutex<StreamState>>) {
    let nals = h264::annex_b_units(&unit.data);
    if nals.is_empty() {
        return;
    }
    let sps = nals
        .iter()
        .find(|nal| h264::nal_type(nal) == h264::NAL_SPS)
        .map(|nal| nal.to_vec());
    let pps = nals
        .iter()
        .find(|nal| h264::nal_type(nal) == h264::NAL_PPS)
        .map(|nal| nal.to_vec());
    let keyframe = nals.iter().any(|nal| h264::nal_type(nal) == h264::NAL_IDR);

    let mut new_parameters = false;
    if let (Some(sps), Some(pps)) = (&sps, &pps) {
        let mut state = lock(state);
        match (h264::avcc_record(sps, pps), h264::dimensions(sps)) {
            (Ok(record), Ok(size)) => {
                new_parameters = state.avcc.as_deref() != Some(record.as_slice());
                if new_parameters {
                    state.codec = h264::rfc6381_codec(sps).ok();
                    state.dimensions = Some(size);
                    state.avcc = Some(record);
                    tracing::info!(key = %key, width = size.0, height = size.1, "SRT stream described itself");
                }
            }
            // A stream this gateway cannot describe can still be watched, so
            // this is not fatal — but it will not record, and that is worth
            // saying once rather than per frame.
            (record, size) => {
                if state.avcc.is_none() {
                    let error = record
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| size.err().map(|e| e.to_string()).unwrap_or_default());
                    tracing::warn!(key = %key, %error, "SRT parameter sets not understood");
                }
            }
        }
    }

    // The recorder wants four-byte lengths and the parameter sets out of band,
    // which is what it gets from every other source too.
    let payload: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|nal| {
            !matches!(
                h264::nal_type(nal),
                h264::NAL_SPS | h264::NAL_PPS | 6 | 9 | 12
            )
        })
        .collect();
    if payload.is_empty() {
        return;
    }
    let data = bytes::Bytes::from(h264::to_avcc(&payload));

    {
        let mut state = lock(state);
        state.frames += 1;
        state.bytes += data.len() as u64;
        state.last_frame = Some(Instant::now());
    }
    ingest.publish(
        key,
        Frame {
            data,
            timestamp: unit.pts.unwrap_or_default(),
            clock_rate: TS_CLOCK_RATE,
            keyframe,
            new_parameters,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::FrameSource as _;
    use futures::SinkExt;
    use std::time::Duration;

    const FIXTURE: &[u8] = include_bytes!("../../fixtures/camera.ts");

    async fn listening(ingest: &Ingest) -> SocketAddr {
        // srt-tokio does not report the port it bound, so take one from the OS
        // and hand it over.
        let bound = {
            let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let (listener, mut incoming) = SrtListener::builder().bind(bound).await.unwrap();
        let ingest = ingest.clone();
        tokio::spawn(async move {
            // Held for the life of the task: dropping it closes the socket.
            let _listener = listener;
            while let Some(request) = incoming.incoming().next().await {
                let key = stream_key(
                    &request
                        .stream_id()
                        .map(|id| id.as_str().to_owned())
                        .unwrap_or_default(),
                );
                if key.is_empty() || !ingest.is_allowed(&key) {
                    let _ = request
                        .reject(RejectReason::Server(ServerRejectReason::Forbidden))
                        .await;
                    continue;
                }
                let ingest = ingest.clone();
                tokio::spawn(async move {
                    let Ok(socket) = request.accept(None).await else {
                        return;
                    };
                    let state = ingest.open(&key);
                    let _ = carry(socket, &ingest, &key, &state).await;
                    ingest.close(&key);
                });
            }
        });
        bound
    }

    /// Send the fixture as an encoder would: 7 transport packets per datagram.
    /// Waits for `ready` first, because frames sent before anyone subscribed
    /// are gone — which is what live video means.
    async fn publish_fixture(
        address: SocketAddr,
        key: &str,
        ready: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
        let mut socket = srt_tokio::SrtSocket::builder()
            .call(address, Some(key))
            .await?;
        let _ = ready.await;
        for chunk in FIXTURE.chunks(ts::PACKET_LEN * 7) {
            socket
                .send((
                    std::time::Instant::now(),
                    bytes::Bytes::copy_from_slice(chunk),
                ))
                .await?;
        }
        socket.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn an_srt_publisher_becomes_a_frame_source() {
        let ingest = Ingest::new();
        ingest.allow(["yard".to_string()]);
        let address = listening(&ingest).await;

        let (ready, wait) = tokio::sync::oneshot::channel();
        let publishing = tokio::spawn({
            let key = "yard".to_string();
            async move { publish_fixture(address, &key, wait).await }
        });

        let mut source = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(source) = ingest.subscribe("yard") {
                    return source;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the SRT publisher never connected");
        let _ = ready.send(());

        let frame = tokio::time::timeout(Duration::from_secs(20), source.next_frame())
            .await
            .expect("no frame arrived")
            .unwrap()
            .expect("the stream ended before a frame");
        assert_eq!(frame.clock_rate, TS_CLOCK_RATE);
        assert_eq!(
            &frame.data[..4],
            &(frame.data.len() as u32 - 4).to_be_bytes(),
            "frames are handed over in the recorder's framing"
        );

        let parameters = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(parameters) = source.parameters() {
                    return parameters;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the stream never described itself");
        assert_eq!(parameters.pixel_dimensions, (320, 240));
        assert_eq!(parameters.extra_data[0], 1, "an AVCC record");

        publishing.await.unwrap().unwrap();
    }

    /// The tests above prove this gateway understands srt-tokio and a
    /// transport stream ffmpeg wrote earlier. This proves it understands
    /// ffmpeg pushing live, which is the thing an encoder does.
    ///
    /// Run with `make check-ingest`.
    #[tokio::test]
    #[ignore = "needs ffmpeg with SRT; run make check-ingest"]
    async fn ffmpeg_can_push_srt_to_this_gateway() {
        let ingest = Ingest::new();
        ingest.allow(["yard".to_string()]);
        let address = listening(&ingest).await;

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/camera.h264");
        let mut encoder = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-re",
                "-f",
                "h264",
                "-i",
                fixture,
                "-c",
                "copy",
                "-f",
                "mpegts",
                &format!("srt://{address}?streamid=yard&mode=caller"),
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
        .expect("ffmpeg never got as far as pushing");

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
        assert!(frames.iter().any(|frame| frame.keyframe), "no keyframe");
        assert!(
            frames.iter().all(|frame| frame.clock_rate == TS_CLOCK_RATE),
            "MPEG-TS counts in 90 kHz"
        );
        let parameters = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(parameters) = source.parameters() {
                    return parameters;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the stream never described itself");
        assert_eq!(parameters.pixel_dimensions, (320, 240));
    }

    #[tokio::test]
    async fn an_srt_stream_key_nobody_registered_is_refused() {
        let ingest = Ingest::new();
        ingest.allow(["yard".to_string()]);
        let address = listening(&ingest).await;

        let refused = tokio::time::timeout(
            Duration::from_secs(20),
            srt_tokio::SrtSocket::builder().call(address, Some("not-a-source")),
        )
        .await
        .expect("the listener never answered");
        assert!(refused.is_err(), "an unlisted key must be refused");
        assert!(ingest.subscribe("not-a-source").is_none());
    }

    #[test]
    fn a_stream_id_can_name_its_resource_either_way() {
        assert_eq!(stream_key("loading-bay"), "loading-bay");
        assert_eq!(stream_key("#!::r=loading-bay,m=publish"), "loading-bay");
        assert_eq!(stream_key("  yard  "), "yard");
    }
}
