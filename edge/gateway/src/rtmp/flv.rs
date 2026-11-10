//! FLV video tags, which is how RTMP carries H.264.
//!
//! A tag is one byte of frame type and codec id, one byte saying whether this
//! is the decoder configuration or an access unit, a 24-bit composition time
//! offset, and then the payload: an AVCDecoderConfigurationRecord for the
//! former, length-prefixed NAL units for the latter. Those lengths are exactly
//! the framing the recorder already writes, so nothing is converted here.

use anyhow::anyhow;
use bytes::Bytes;

/// What a video tag turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoTag {
    /// The decoder configuration: an AVCDecoderConfigurationRecord.
    Parameters(Vec<u8>),
    /// One access unit, in AVCC framing.
    Frame { data: Bytes, keyframe: bool },
    /// The publisher said the stream is over.
    End,
}

const CODEC_AVC: u8 = 7;

pub fn parse_video_tag(data: &Bytes) -> anyhow::Result<VideoTag> {
    let header = *data.first().ok_or_else(|| anyhow!("empty FLV video tag"))?;
    let codec = header & 0x0f;
    if codec != CODEC_AVC {
        return Err(anyhow!(
            "this gateway carries H.264 without transcoding; the publisher sent FLV codec {codec}"
        ));
    }
    let keyframe = (header >> 4) == 1;
    let packet_type = *data
        .get(1)
        .ok_or_else(|| anyhow!("truncated FLV video tag"))?;
    if data.len() < 5 {
        return Err(anyhow!("truncated FLV video tag"));
    }
    match packet_type {
        0 => Ok(VideoTag::Parameters(data[5..].to_vec())),
        1 => Ok(VideoTag::Frame {
            data: data.slice(5..),
            keyframe,
        }),
        2 => Ok(VideoTag::End),
        other => Err(anyhow!("unknown AVC packet type {other} in FLV video tag")),
    }
}

/// The codec string for an fMP4 track, from the first four bytes of an
/// AVCDecoderConfigurationRecord. RTSP sources get this from the SDP; a
/// publisher only ever sends the record.
pub fn rfc6381_codec(avcc: &[u8]) -> anyhow::Result<String> {
    if avcc.len() < 4 || avcc[0] != 1 {
        return Err(anyhow!("invalid AVCDecoderConfigurationRecord"));
    }
    Ok(format!(
        "avc1.{:02x}{:02x}{:02x}",
        avcc[1], avcc[2], avcc[3]
    ))
}

/// The SPS and PPS an AVCDecoderConfigurationRecord carries. A browser will
/// not decode a stream without them, and FLV sends them once, out of band.
pub fn parameter_sets(avcc: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    if avcc.len() < 6 || avcc[0] != 1 {
        return Err(anyhow!("invalid AVCDecoderConfigurationRecord"));
    }
    let mut sets = Vec::new();
    let mut at = 5;
    // Two runs, each counted in the low bits of its own byte: SPS then PPS.
    for (count_mask, counted_at) in [(0x1f_u8, 5_usize), (0xff, usize::MAX)] {
        let (count, from) = if counted_at == usize::MAX {
            let Some(&byte) = avcc.get(at) else { break };
            at += 1;
            (byte & count_mask, at)
        } else {
            (avcc[counted_at] & count_mask, 6)
        };
        at = at.max(from);
        for _ in 0..count {
            let Some(length) = avcc
                .get(at..at + 2)
                .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            else {
                return Err(anyhow!("truncated parameter set length"));
            };
            at += 2;
            let Some(set) = avcc.get(at..at + length) else {
                return Err(anyhow!("truncated parameter set"));
            };
            sets.push(set.to_vec());
            at += length;
        }
    }
    Ok(sets)
}

/// How many bytes each NALU's length takes in this stream: the record says,
/// and four is what every encoder seen here uses.
pub fn nalu_length_size(avcc: &[u8]) -> usize {
    avcc.get(4).map_or(4, |byte| usize::from((byte & 0x03) + 1))
}

/// Length-prefixed NALUs to Annex B start codes, with the parameter sets in
/// front of a key frame so a decoder can start on it. A NALU whose length runs
/// past the end of the frame is dropped rather than guessed at.
pub fn to_annex_b(
    frame: &[u8],
    length_size: usize,
    parameter_sets: &[Vec<u8>],
    keyframe: bool,
) -> Vec<u8> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(frame.len() + 64);
    if keyframe {
        for set in parameter_sets {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
    }
    let mut at = 0;
    while at + length_size <= frame.len() {
        let mut length = 0usize;
        for byte in &frame[at..at + length_size] {
            length = (length << 8) | usize::from(*byte);
        }
        at += length_size;
        let Some(nalu) = frame.get(at..at + length) else {
            break;
        };
        out.extend_from_slice(&START);
        out.extend_from_slice(nalu);
        at += length;
    }
    out
}

#[cfg(test)]
mod tests {
    /// A browser needs Annex B with the parameter sets in band. FLV carries
    /// neither: NALUs come length-prefixed and SPS/PPS arrive once, in the
    /// decoder configuration. Sending the payload as it arrives gave a browser
    /// 288 KB and no decoded frame at all.
    #[test]
    fn a_pushed_frame_becomes_annex_b_with_its_parameter_sets() {
        // One SPS (0x67 ...), one PPS (0x68 ...), 4-byte NALU lengths.
        let avcc = vec![
            1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 3, 0x67, 0x42, 0x1e, 1, 0, 2, 0x68, 0xce,
        ];
        let sets = parameter_sets(&avcc).expect("a record this shape parses");
        assert_eq!(sets, vec![vec![0x67, 0x42, 0x1e], vec![0x68, 0xce]]);
        assert_eq!(nalu_length_size(&avcc), 4);

        let frame = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        let start = [0u8, 0, 0, 1];

        let key = to_annex_b(&frame, 4, &sets, true);
        assert_eq!(
            key,
            [
                &start[..],
                &[0x67, 0x42, 0x1e],
                &start[..],
                &[0x68, 0xce],
                &start[..],
                &[0x65, 0xaa],
                &start[..],
                &[0x41],
            ]
            .concat(),
            "a key frame carries the parameter sets before it"
        );

        let inter = to_annex_b(&frame, 4, &sets, false);
        assert_eq!(
            inter,
            [&start[..], &[0x65, 0xaa], &start[..], &[0x41]].concat(),
            "an inter frame repeats nothing"
        );
    }

    use super::*;

    #[test]
    fn a_sequence_header_carries_the_decoder_configuration() {
        let tag = Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x42, 0xe0, 0x1e, 0xff]);
        assert_eq!(
            parse_video_tag(&tag).unwrap(),
            VideoTag::Parameters(vec![1, 0x42, 0xe0, 0x1e, 0xff])
        );
    }

    #[test]
    fn a_keyframe_tag_is_a_keyframe_and_its_bytes_are_untouched() {
        let tag = Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88]);
        assert_eq!(
            parse_video_tag(&tag).unwrap(),
            VideoTag::Frame {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]),
                keyframe: true,
            }
        );
    }

    #[test]
    fn an_inter_frame_is_not_a_keyframe() {
        let tag = Bytes::from_static(&[0x27, 0x01, 0, 0, 0, 0, 0, 0, 1, 0x41]);
        let VideoTag::Frame { keyframe, .. } = parse_video_tag(&tag).unwrap() else {
            panic!("expected a frame");
        };
        assert!(!keyframe);
    }

    #[test]
    fn a_publisher_that_is_not_h264_is_refused_by_name() {
        // VP6, which some encoders still offer. Refusing here beats recording
        // bytes no player will decode.
        let tag = Bytes::from_static(&[0x14, 0x01, 0, 0, 0]);
        let error = parse_video_tag(&tag).unwrap_err().to_string();
        assert!(error.contains("H.264"), "{error}");
        assert!(error.contains("codec 4"), "{error}");
    }

    #[test]
    fn a_truncated_tag_is_an_error_rather_than_a_panic() {
        for length in 0..5 {
            let tag = Bytes::from(vec![0x17; length]);
            assert!(parse_video_tag(&tag).is_err(), "length {length}");
        }
    }

    #[test]
    fn the_codec_string_comes_from_the_configuration_record() {
        assert_eq!(
            rfc6381_codec(&[1, 0x42, 0xe0, 0x1e, 0xff]).unwrap(),
            "avc1.42e01e"
        );
        assert!(rfc6381_codec(&[0, 1, 2, 3]).is_err());
    }
}
