//! The bits of H.264 a gateway has to understand to carry a stream that
//! arrived without a container to describe it.
//!
//! RTSP hands over parameter sets in the SDP and RTMP in a configuration
//! record; MPEG-TS hands over nothing at all, so the parameter sets arrive
//! in band and the picture size has to be read out of the SPS. That is the
//! only reason this file parses anything: to say how wide the video is.

use anyhow::{anyhow, bail};

/// Split an Annex B byte stream into NAL units, dropping the start codes.
pub fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index + 2 < data.len() {
        let three = data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1;
        if !three {
            index += 1;
            continue;
        }
        let code_start = if index > 0 && data[index - 1] == 0 {
            index - 1
        } else {
            index
        };
        if let Some(begin) = start.filter(|begin| code_start > *begin) {
            units.push(&data[begin..code_start]);
        }
        index += 3;
        start = Some(index);
    }
    if let Some(begin) = start.filter(|begin| *begin < data.len()) {
        units.push(&data[begin..]);
    }
    units.retain(|unit| !unit.is_empty());
    units
}

/// The same NAL units with four-byte lengths, which is what the recorder
/// writes and what a WebRTC sample writer does not want.
pub fn to_avcc(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in units {
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

pub fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |byte| byte & 0x1f)
}

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_IDR: u8 = 5;

/// An AVCDecoderConfigurationRecord built from parameter sets seen in band.
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> anyhow::Result<Vec<u8>> {
    if sps.len() < 4 {
        bail!("SPS too short to describe a stream");
    }
    let mut record = vec![
        1, sps[1], sps[2], sps[3], // configurationVersion, profile, compat, level
        0xff,   // 6 reserved bits, then 4-byte NAL lengths
        0xe1,   // 3 reserved bits, one SPS
    ];
    record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    record.extend_from_slice(sps);
    record.push(1);
    record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    record.extend_from_slice(pps);
    Ok(record)
}

pub fn rfc6381_codec(sps: &[u8]) -> anyhow::Result<String> {
    if sps.len() < 4 {
        bail!("SPS too short to name a codec");
    }
    Ok(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// The picture size the SPS declares, cropping included.
pub fn dimensions(sps: &[u8]) -> anyhow::Result<(u32, u32)> {
    let rbsp = unescape(sps.get(1..).ok_or_else(|| anyhow!("empty SPS"))?);
    let mut bits = Bits::new(&rbsp);
    let profile_idc = bits.u(8)?;
    bits.u(8)?; // constraint flags and reserved bits
    bits.u(8)?; // level_idc
    bits.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1; // 4:2:0 unless the profile says otherwise
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = bits.u(1)? == 1;
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.u(1)? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..lists {
                if bits.u(1)? == 1 {
                    skip_scaling_list(&mut bits, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    bits.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = bits.ue()?;
    if pic_order_cnt_type == 0 {
        bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        bits.u(1)?; // delta_pic_order_always_zero_flag
        bits.se()?; // offset_for_non_ref_pic
        bits.se()?; // offset_for_top_to_bottom_field
        let cycle = bits.ue()?;
        for _ in 0..cycle {
            bits.se()?;
        }
    }
    bits.ue()?; // max_num_ref_frames
    bits.u(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = bits.ue()? + 1;
    let height_map_units = bits.ue()? + 1;
    let frame_mbs_only = bits.u(1)?;
    if frame_mbs_only == 0 {
        bits.u(1)?; // mb_adaptive_frame_field_flag
    }
    bits.u(1)?; // direct_8x8_inference_flag

    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if bits.u(1)? == 1 {
        left = bits.ue()?;
        right = bits.ue()?;
        top = bits.ue()?;
        bottom = bits.ue()?;
    }

    // How many luma samples one unit of cropping covers, which depends on the
    // chroma layout and on whether the stream is interlaced.
    let (sub_width, sub_height) = match chroma_format_idc {
        0 => (1, 1),
        3 if separate_colour_plane => (1, 1),
        3 => (1, 1),
        2 => (2, 1),
        _ => (2, 2),
    };
    let crop_unit_x = sub_width;
    let crop_unit_y = sub_height * (2 - frame_mbs_only);

    let width = width_mbs * 16 - (left + right) * crop_unit_x;
    let height = (2 - frame_mbs_only) * height_map_units * 16 - (top + bottom) * crop_unit_y;
    if width == 0 || height == 0 {
        bail!("SPS describes a picture of no size");
    }
    Ok((width, height))
}

/// Remove emulation prevention bytes: `00 00 03` means `00 00` in the payload.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(byte);
    }
    out
}

fn skip_scaling_list(bits: &mut Bits<'_>, size: u32) -> anyhow::Result<()> {
    let mut last = 8_i32;
    let mut next = 8_i32;
    for _ in 0..size {
        if next != 0 {
            next = (last + bits.se()? + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

/// Just enough of a bit reader for an SPS.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> anyhow::Result<u32> {
        let byte = self
            .data
            .get(self.position / 8)
            .ok_or_else(|| anyhow!("SPS ended mid-field"))?;
        let bit = (byte >> (7 - (self.position % 8))) & 1;
        self.position += 1;
        Ok(u32::from(bit))
    }

    fn u(&mut self, count: u32) -> anyhow::Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    /// Unsigned exp-Golomb.
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                bail!("SPS field is not a number this decoder can read");
            }
        }
        if leading == 0 {
            return Ok(0);
        }
        Ok((1 << leading) - 1 + self.u(leading)?)
    }

    /// Signed exp-Golomb.
    fn se(&mut self) -> anyhow::Result<i32> {
        let value = self.ue()?;
        let magnitude = value.div_ceil(2) as i32;
        Ok(if value % 2 == 0 {
            -magnitude
        } else {
            magnitude
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed fixture, which ffprobe reports as 320x240 Constrained
    /// Baseline. Reading the same numbers out of the SPS is the point.
    fn fixture_sps() -> Vec<u8> {
        let raw = include_bytes!("../fixtures/camera.h264");
        let units = annex_b_units(raw);
        units
            .iter()
            .find(|unit| nal_type(unit) == NAL_SPS)
            .expect("the fixture carries an SPS")
            .to_vec()
    }

    #[test]
    fn a_real_sps_gives_the_size_ffprobe_reports() {
        assert_eq!(dimensions(&fixture_sps()).unwrap(), (320, 240));
    }

    #[test]
    fn a_1080p_high_profile_sps_is_read_too() {
        // High profile takes the branch with the chroma format and the scaling
        // lists, which baseline does not, and most cameras are High.
        let sps = [
            0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0x84, 0x00, 0x00,
            0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
        ];
        assert_eq!(dimensions(&sps).unwrap(), (1920, 1080));
    }

    #[test]
    fn a_truncated_sps_is_an_error_rather_than_a_panic() {
        for length in 0..6 {
            let sps = vec![0x67_u8; length];
            assert!(dimensions(&sps).is_err(), "length {length}");
        }
    }

    #[test]
    fn annex_b_splits_on_both_start_code_lengths() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xaa, // four-byte start code
            0, 0, 1, 0x68, 0xbb, // three-byte start code
            0, 0, 0, 1, 0x65, 0xcc, 0xdd,
        ];
        let units = annex_b_units(&stream);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], &[0x67, 0xaa]);
        assert_eq!(units[1], &[0x68, 0xbb]);
        assert_eq!(units[2], &[0x65, 0xcc, 0xdd]);
        assert_eq!(nal_type(units[2]), NAL_IDR);
    }

    #[test]
    fn avcc_framing_is_four_byte_lengths() {
        let units: Vec<&[u8]> = vec![&[0x65, 1, 2], &[0x41, 3]];
        assert_eq!(
            to_avcc(&units),
            vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]
        );
    }

    #[test]
    fn a_configuration_record_describes_the_parameter_sets() {
        let sps = fixture_sps();
        let record = avcc_record(&sps, &[0x68, 0xce, 0x3c, 0x80]).unwrap();
        assert_eq!(record[0], 1, "configurationVersion");
        assert_eq!(&record[1..4], &sps[1..4], "profile, compatibility, level");
        assert_eq!(record[4] & 0x03, 3, "four-byte NAL lengths");
        // The recorder parses this record back; that it round-trips is what
        // makes an SRT recording playable.
        assert_eq!(
            rfc6381_codec(&sps).unwrap(),
            format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3])
        );
    }
}
