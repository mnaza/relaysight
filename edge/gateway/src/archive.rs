use std::{num::NonZeroU32, time::Duration};

use anyhow::{Context, anyhow};
use futures::StreamExt;
use retina::{
    client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions},
    codec::{CodecItem, FrameFormat, ParametersRef},
};
use shiguredo_mp4::{
    TrackKind, Uint,
    boxes::{Avc1Box, AvccBox, SampleEntry, VisualSampleEntryFields},
    mux::{Fmp4SegmentMuxer, Sample},
};
use tokio::time::timeout;
use url::Url;

#[derive(Debug, Clone)]
pub struct CmafSegment {
    pub sequence: u32,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CmafRecording {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub init: Vec<u8>,
    pub segments: Vec<CmafSegment>,
}

#[derive(Debug)]
struct EncodedFrame {
    timestamp: i64,
    data: Vec<u8>,
    keyframe: bool,
}

#[derive(Debug)]
struct AvccConfig {
    profile: u8,
    compatibility: u8,
    level: u8,
    length_size_minus_one: u8,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

pub async fn record_h264_cmaf(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    total_duration: Duration,
    target_segment_duration: Duration,
) -> anyhow::Result<CmafRecording> {
    let (url, creds) = clean_rtsp_url(raw_url, username, password)?;
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
    let encoding = session.streams()[video_stream]
        .encoding_name()
        .to_ascii_uppercase();
    if encoding != "H264" {
        return Err(anyhow!(
            "CMAF recorder currently supports H264, camera returned {encoding}"
        ));
    }

    timeout(
        Duration::from_secs(8),
        session.setup(
            video_stream,
            SetupOptions::default().frame_format(FrameFormat::MP4),
        ),
    )
    .await
    .context("RTSP SETUP timeout")??;

    let playing = timeout(Duration::from_secs(8), session.play(PlayOptions::default()))
        .await
        .context("RTSP PLAY timeout")??;
    let mut demuxed = playing.demuxed()?;

    let receive_deadline = total_duration + Duration::from_secs(12);
    let receive_started = tokio::time::Instant::now();
    let mut first_timestamp = None;
    let mut clock_rate = None;
    let mut frames: Vec<EncodedFrame> = Vec::new();
    let mut codec = None;
    let mut dimensions = None;
    let mut avcc = None;
    let mut fallback_duration = None;

    while receive_started.elapsed() < receive_deadline {
        let item = timeout(Duration::from_secs(4), demuxed.next())
            .await
            .context("RTSP frame timeout")?;
        let Some(item) = item else { break };
        let item = item.context("receive RTSP media")?;
        let CodecItem::VideoFrame(frame) = item else {
            continue;
        };
        if frame.stream_id() != video_stream {
            continue;
        }

        // A recording must start at a random access point so it can be decoded independently.
        if first_timestamp.is_none() && !frame.is_random_access_point() {
            continue;
        }

        if first_timestamp.is_none() || frame.has_new_parameters() {
            let params = match demuxed.streams()[video_stream].parameters() {
                Some(ParametersRef::Video(params)) => params,
                _ => return Err(anyhow!("video parameters unavailable after H264 frame")),
            };
            let new_codec = params.rfc6381_codec().to_owned();
            let new_dimensions = params.pixel_dimensions();
            let new_avcc = parse_avcc(params.extra_data())?;
            if first_timestamp.is_some()
                && (codec.as_ref() != Some(&new_codec) || dimensions != Some(new_dimensions))
            {
                return Err(anyhow!(
                    "camera changed H264 parameters during recording; start a new recording"
                ));
            }
            codec = Some(new_codec);
            dimensions = Some(new_dimensions);
            avcc = Some(new_avcc);
            fallback_duration = params.frame_rate().and_then(|(num, den)| {
                if den == 0 {
                    None
                } else {
                    let rate = frame.timestamp().clock_rate().get() as f64;
                    Some(((rate * num as f64 / den as f64).round() as u32).max(1))
                }
            });
        }

        let ts = frame.timestamp();
        if first_timestamp.is_none() {
            first_timestamp = Some(ts.timestamp());
            clock_rate = Some(ts.clock_rate());
        }
        if Some(ts.clock_rate()) != clock_rate {
            return Err(anyhow!("RTSP video clock rate changed during recording"));
        }
        let start = first_timestamp.expect("set above");
        let elapsed_ticks = ts.timestamp().saturating_sub(start).max(0) as u64;
        let elapsed = Duration::from_secs_f64(elapsed_ticks as f64 / ts.clock_rate().get() as f64);

        let keyframe = frame.is_random_access_point();
        frames.push(EncodedFrame {
            timestamp: ts.timestamp(),
            data: frame.into_data(),
            keyframe,
        });

        // Continue until a keyframe at/after requested duration. This makes the final
        // media segment naturally close on a GOP boundary without decoding frames.
        if elapsed >= total_duration
            && frames.len() > 1
            && frames.last().is_some_and(|f| f.keyframe)
        {
            break;
        }
    }

    if frames.len() < 2 {
        return Err(anyhow!("not enough H264 frames to build fMP4 recording"));
    }
    let clock_rate = clock_rate.ok_or_else(|| anyhow!("missing RTP clock rate"))?;
    let codec = codec.ok_or_else(|| anyhow!("missing H264 codec parameters"))?;
    let (width, height) = dimensions.ok_or_else(|| anyhow!("missing video dimensions"))?;
    let avcc = avcc.ok_or_else(|| anyhow!("missing AVCDecoderConfigurationRecord"))?;
    let sample_entry = create_avc1_sample_entry(width, height, &avcc)?;

    let mut durations = Vec::with_capacity(frames.len());
    let mut last_good = fallback_duration.unwrap_or_else(|| (clock_rate.get() / 25).max(1));
    for pair in frames.windows(2) {
        let delta = pair[1].timestamp.saturating_sub(pair[0].timestamp);
        if delta > 0 {
            last_good = u32::try_from(delta).unwrap_or(u32::MAX).max(1);
        }
        durations.push(last_good);
    }
    durations.push(last_good);

    let target_ticks = (target_segment_duration.as_secs_f64() * clock_rate.get() as f64)
        .round()
        .max(1.0) as i64;
    let groups = split_on_keyframes(&frames, target_ticks);
    let first_ts = frames[0].timestamp;
    let mut muxer = Fmp4SegmentMuxer::new()?;
    let mut segments = Vec::with_capacity(groups.len());

    for (sequence, (start_idx, end_idx)) in groups.into_iter().enumerate() {
        let mut samples = Vec::with_capacity(end_idx - start_idx);
        let mut payloads: Vec<&[u8]> = Vec::with_capacity(end_idx - start_idx);
        let mut data_offset = 0_u64;
        let mut segment_ticks = 0_u64;
        for idx in start_idx..end_idx {
            let frame = &frames[idx];
            let duration = durations[idx];
            samples.push(Sample {
                track_kind: TrackKind::Video,
                timescale: clock_rate,
                sample_entry: Some(sample_entry.clone()),
                duration,
                keyframe: frame.keyframe,
                composition_time_offset: None,
                data_offset,
                data_size: frame.data.len(),
            });
            payloads.push(&frame.data);
            data_offset = data_offset.saturating_add(frame.data.len() as u64);
            segment_ticks = segment_ticks.saturating_add(duration as u64);
        }
        let metadata = muxer.create_media_segment_metadata(&samples)?;
        let mut bytes = metadata;
        bytes.reserve(data_offset as usize);
        for payload in payloads {
            bytes.extend_from_slice(payload);
        }

        let start_ticks = frames[start_idx].timestamp.saturating_sub(first_ts).max(0) as u64;
        segments.push(CmafSegment {
            sequence: sequence as u32,
            start_offset_ms: ticks_to_ms(start_ticks, clock_rate),
            duration_ms: ticks_to_ms(segment_ticks, clock_rate),
            bytes,
        });
    }

    let init = muxer.init_segment_bytes()?;
    Ok(CmafRecording {
        codec,
        width,
        height,
        init,
        segments,
    })
}

fn clean_rtsp_url(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<(Url, Option<Credentials>)> {
    let mut url = Url::parse(raw_url).context("parse RTSP URL")?;
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
    Ok((url, creds))
}

fn split_on_keyframes(frames: &[EncodedFrame], target_ticks: i64) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0_usize;
    for idx in 1..frames.len() {
        let elapsed = frames[idx]
            .timestamp
            .saturating_sub(frames[start].timestamp);
        if elapsed >= target_ticks && frames[idx].keyframe {
            groups.push((start, idx));
            start = idx;
        }
    }
    if start < frames.len() {
        groups.push((start, frames.len()));
    }
    groups.retain(|(start, end)| end > start);
    groups
}

fn ticks_to_ms(ticks: u64, clock_rate: NonZeroU32) -> u64 {
    ((ticks as u128 * 1000) / clock_rate.get() as u128) as u64
}

fn create_avc1_sample_entry(
    width: u32,
    height: u32,
    avcc: &AvccConfig,
) -> anyhow::Result<SampleEntry> {
    let width = u16::try_from(width).context("video width exceeds MP4 avc1 field")?;
    let height = u16::try_from(height).context("video height exceeds MP4 avc1 field")?;
    Ok(SampleEntry::Avc1(Avc1Box {
        visual: VisualSampleEntryFields {
            data_reference_index: VisualSampleEntryFields::DEFAULT_DATA_REFERENCE_INDEX,
            width,
            height,
            horizresolution: VisualSampleEntryFields::DEFAULT_HORIZRESOLUTION,
            vertresolution: VisualSampleEntryFields::DEFAULT_VERTRESOLUTION,
            frame_count: VisualSampleEntryFields::DEFAULT_FRAME_COUNT,
            compressorname: VisualSampleEntryFields::NULL_COMPRESSORNAME,
            depth: VisualSampleEntryFields::DEFAULT_DEPTH,
        },
        avcc_box: AvccBox {
            avc_profile_indication: avcc.profile,
            profile_compatibility: avcc.compatibility,
            avc_level_indication: avcc.level,
            length_size_minus_one: Uint::new(avcc.length_size_minus_one),
            sps_list: avcc.sps.clone(),
            pps_list: avcc.pps.clone(),
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            sps_ext_list: vec![],
        },
        unknown_boxes: vec![],
    }))
}

fn parse_avcc(data: &[u8]) -> anyhow::Result<AvccConfig> {
    if data.len() < 7 || data[0] != 1 {
        return Err(anyhow!("invalid AVCDecoderConfigurationRecord"));
    }
    let profile = data[1];
    let compatibility = data[2];
    let level = data[3];
    let length_size_minus_one = data[4] & 0x03;
    if length_size_minus_one != 3 {
        return Err(anyhow!("only 4-byte H264 NAL lengths are supported"));
    }
    let mut cursor = 6_usize;
    let sps_count = (data[5] & 0x1f) as usize;
    let mut sps = Vec::with_capacity(sps_count);
    for _ in 0..sps_count {
        sps.push(read_avcc_nal(data, &mut cursor)?);
    }
    let pps_count = *data
        .get(cursor)
        .ok_or_else(|| anyhow!("AVCC missing PPS count"))? as usize;
    cursor += 1;
    let mut pps = Vec::with_capacity(pps_count);
    for _ in 0..pps_count {
        pps.push(read_avcc_nal(data, &mut cursor)?);
    }
    if sps.is_empty() || pps.is_empty() {
        return Err(anyhow!("AVCC has no SPS/PPS"));
    }
    Ok(AvccConfig {
        profile,
        compatibility,
        level,
        length_size_minus_one,
        sps,
        pps,
    })
}

fn read_avcc_nal(data: &[u8], cursor: &mut usize) -> anyhow::Result<Vec<u8>> {
    let len_bytes = data
        .get(*cursor..*cursor + 2)
        .ok_or_else(|| anyhow!("AVCC truncated NAL length"))?;
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    *cursor += 2;
    let nal = data
        .get(*cursor..*cursor + len)
        .ok_or_else(|| anyhow!("AVCC truncated NAL"))?
        .to_vec();
    *cursor += len;
    Ok(nal)
}

#[cfg(test)]
mod tests {
    use super::{EncodedFrame, parse_avcc, split_on_keyframes};

    #[test]
    fn parses_avcc_sps_pps() {
        let data = [
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 4, 0x68, 0xee, 0x3c,
            0x80,
        ];
        let avcc = parse_avcc(&data).unwrap();
        assert_eq!(avcc.profile, 0x64);
        assert_eq!(avcc.sps.len(), 1);
        assert_eq!(avcc.pps.len(), 1);
        assert_eq!(avcc.length_size_minus_one, 3);
    }

    #[test]
    fn segments_only_on_keyframes() {
        let frames: Vec<_> = [
            (0, true),
            (3000, false),
            (6000, false),
            (9000, true),
            (12000, false),
            (15000, false),
            (18000, true),
        ]
        .into_iter()
        .map(|(timestamp, keyframe)| EncodedFrame {
            timestamp,
            data: vec![1],
            keyframe,
        })
        .collect();
        assert_eq!(
            split_on_keyframes(&frames, 8000),
            vec![(0, 3), (3, 6), (6, 7)]
        );
    }
}
