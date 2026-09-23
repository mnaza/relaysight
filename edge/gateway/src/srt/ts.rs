//! MPEG-TS, down to the one thing a gateway needs from it: H.264 access units.
//!
//! This reads 188-byte packets, follows the PAT to a PMT and the PMT to the
//! first H.264 stream, reassembles that stream's PES packets and hands over
//! each access unit with its presentation timestamp. Everything else in a
//! transport stream — other programmes, audio, sections it did not ask for,
//! scrambling — is skipped rather than parsed.
//!
//! Hand-rolled rather than taken from a crate because the crates that do this
//! properly are built around push-style filter machinery, and this needs one
//! stream, one direction, no configuration. The test vector is a transport
//! stream ffmpeg produced, not one written here.

use anyhow::{Result, anyhow, bail};

pub const PACKET_LEN: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0;
const STREAM_TYPE_H264: u8 = 0x1b;

/// One H.264 access unit, in Annex B, as the stream carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    /// 90 kHz, which is what MPEG-TS counts in. `None` when the PES carried no
    /// PTS, which happens and is not an error.
    pub pts: Option<i64>,
}

/// Feeds on transport packets and yields access units.
#[derive(Default)]
pub struct Demuxer {
    /// Bytes of a packet that arrived split across two reads.
    partial: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pes: Vec<u8>,
    pes_pts: Option<i64>,
    ready: Vec<AccessUnit>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push received bytes; take whatever access units they completed.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<AccessUnit>> {
        let mut bytes = if self.partial.is_empty() {
            data.to_vec()
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(data);
            joined
        };
        let whole = bytes.len() / PACKET_LEN * PACKET_LEN;
        self.partial = bytes.split_off(whole);
        for packet in bytes.as_chunks::<PACKET_LEN>().0 {
            self.packet(packet)?;
        }
        Ok(std::mem::take(&mut self.ready))
    }

    /// No more is coming: give up the access unit still being assembled.
    pub fn flush(&mut self) -> Vec<AccessUnit> {
        self.finish_pes();
        std::mem::take(&mut self.ready)
    }

    fn packet(&mut self, packet: &[u8]) -> Result<()> {
        if packet[0] != SYNC_BYTE {
            bail!("transport stream lost sync");
        }
        // Transport error indicator: the sender says this packet is damaged.
        if packet[1] & 0x80 != 0 {
            return Ok(());
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let payload_start = packet[1] & 0x40 != 0;
        let adaptation = (packet[3] >> 4) & 0x03;
        if packet[3] & 0xc0 != 0 {
            // Scrambled. Nothing here can decrypt it, and guessing would put
            // noise in a recording.
            bail!("transport stream is scrambled");
        }
        let mut offset = 4;
        if adaptation & 0x02 != 0 {
            let length = usize::from(*packet.get(4).ok_or_else(|| anyhow!("short packet"))?);
            offset = 5 + length;
        }
        if adaptation & 0x01 == 0 || offset >= PACKET_LEN {
            return Ok(());
        }
        let payload = &packet[offset..];

        if pid == PAT_PID {
            if payload_start {
                self.pmt_pid = parse_pat(section(payload)?);
            }
            return Ok(());
        }
        if Some(pid) == self.pmt_pid {
            if payload_start {
                self.video_pid = parse_pmt(section(payload)?);
            }
            return Ok(());
        }
        if Some(pid) != self.video_pid {
            return Ok(());
        }

        if payload_start {
            // A new PES packet starts here, so the previous one is complete.
            self.finish_pes();
            let (pts, body) = parse_pes_header(payload)?;
            self.pes_pts = pts;
            self.pes.extend_from_slice(body);
        } else if !self.pes.is_empty() || self.pes_pts.is_some() {
            self.pes.extend_from_slice(payload);
        }
        Ok(())
    }

    fn finish_pes(&mut self) {
        if self.pes.is_empty() {
            self.pes_pts = None;
            return;
        }
        self.ready.push(AccessUnit {
            data: std::mem::take(&mut self.pes),
            pts: self.pes_pts.take(),
        });
    }
}

/// Strip the pointer field that precedes a PSI section.
fn section(payload: &[u8]) -> Result<&[u8]> {
    let pointer = usize::from(*payload.first().ok_or_else(|| anyhow!("empty section"))?);
    payload
        .get(1 + pointer..)
        .ok_or_else(|| anyhow!("section pointer past the packet"))
}

/// The PID of the first programme's PMT.
fn parse_pat(section: &[u8]) -> Option<u16> {
    if section.first()? != &0x00 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([
        section.get(1)? & 0x0f,
        *section.get(2)?,
    ]));
    let end = 3 + length.checked_sub(4)?; // the CRC is not part of the entries
    let entries = section.get(8..end.min(section.len()))?;
    entries.as_chunks::<4>().0.iter().find_map(|entry| {
        let program = u16::from_be_bytes([entry[0], entry[1]]);
        // Programme 0 is the network information table, not a programme.
        (program != 0).then(|| (u16::from(entry[2] & 0x1f) << 8) | u16::from(entry[3]))
    })
}

/// The PID of the first H.264 stream this programme carries.
fn parse_pmt(section: &[u8]) -> Option<u16> {
    if section.first()? != &0x02 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([
        section.get(1)? & 0x0f,
        *section.get(2)?,
    ]));
    let end = (3 + length.checked_sub(4)?).min(section.len());
    let info_length = usize::from(u16::from_be_bytes([
        section.get(10)? & 0x0f,
        *section.get(11)?,
    ]));
    let mut index = 12 + info_length;
    while index + 5 <= end {
        let stream_type = section[index];
        let pid = (u16::from(section[index + 1] & 0x1f) << 8) | u16::from(section[index + 2]);
        let descriptors = usize::from(u16::from_be_bytes([
            section[index + 3] & 0x0f,
            section[index + 4],
        ]));
        if stream_type == STREAM_TYPE_H264 {
            return Some(pid);
        }
        index += 5 + descriptors;
    }
    None
}

/// The PTS, if this PES packet carries one, and the payload after the header.
fn parse_pes_header(payload: &[u8]) -> Result<(Option<i64>, &[u8])> {
    if payload.len() < 9 || payload[0..3] != [0, 0, 1] {
        bail!("PES packet without a start code");
    }
    let header_length = usize::from(payload[8]);
    let body = payload
        .get(9 + header_length..)
        .ok_or_else(|| anyhow!("PES header longer than the packet"))?;
    let flags = payload[7] >> 6;
    if flags & 0x02 == 0 {
        return Ok((None, body));
    }
    let pts = payload
        .get(9..14)
        .ok_or_else(|| anyhow!("PES claims a PTS it did not carry"))?;
    let value = (i64::from(pts[0] & 0x0e) << 29)
        | (i64::from(pts[1]) << 22)
        | (i64::from(pts[2] & 0xfe) << 14)
        | (i64::from(pts[3]) << 7)
        | (i64::from(pts[4]) >> 1);
    Ok((Some(value), body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264;

    /// A transport stream ffmpeg made from the committed H.264 fixture. Using a
    /// real muxer's output is the whole point: a stream written by this file's
    /// own assumptions would prove nothing.
    const FIXTURE: &[u8] = include_bytes!("../../fixtures/camera.ts");

    #[test]
    fn a_real_transport_stream_yields_its_access_units() {
        let mut demuxer = Demuxer::new();
        let mut units = demuxer.push(FIXTURE).unwrap();
        units.extend(demuxer.flush());

        assert!(units.len() > 10, "got {} access units", units.len());
        assert!(
            units.iter().all(|unit| unit.pts.is_some()),
            "ffmpeg timestamps every access unit"
        );
        let timestamps: Vec<i64> = units.iter().filter_map(|unit| unit.pts).collect();
        assert!(
            timestamps.windows(2).all(|pair| pair[1] > pair[0]),
            "timestamps move forward"
        );
    }

    #[test]
    fn the_parameter_sets_come_out_readable() {
        // What makes an SRT recording possible: the SPS is in band, and it says
        // the same size ffprobe reports for the fixture.
        let mut demuxer = Demuxer::new();
        let units = demuxer.push(FIXTURE).unwrap();
        let first = &units.first().expect("at least one access unit").data;
        let nals = h264::annex_b_units(first);
        let sps = nals
            .iter()
            .find(|nal| h264::nal_type(nal) == h264::NAL_SPS)
            .expect("the first access unit carries an SPS");
        assert_eq!(h264::dimensions(sps).unwrap(), (320, 240));
        assert!(
            nals.iter().any(|nal| h264::nal_type(nal) == h264::NAL_IDR),
            "and a keyframe to start on"
        );
    }

    #[test]
    fn a_stream_arriving_in_odd_sized_reads_is_the_same_stream() {
        // SRT delivers datagrams, not packet-aligned reads.
        let mut whole = Demuxer::new();
        let mut expected = whole.push(FIXTURE).unwrap();
        expected.extend(whole.flush());

        let mut split = Demuxer::new();
        let mut got = Vec::new();
        for chunk in FIXTURE.chunks(377) {
            got.extend(split.push(chunk).unwrap());
        }
        got.extend(split.flush());
        assert_eq!(got, expected);
    }

    #[test]
    fn something_that_is_not_a_transport_stream_is_refused() {
        let mut demuxer = Demuxer::new();
        let noise = vec![0x41_u8; PACKET_LEN * 2];
        assert!(demuxer.push(&noise).is_err());
    }

    #[test]
    fn a_scrambled_stream_is_refused_rather_than_recorded_as_noise() {
        let mut packet = vec![0_u8; PACKET_LEN];
        packet[0] = SYNC_BYTE;
        packet[3] = 0xd0; // transport_scrambling_control set, payload present
        let mut demuxer = Demuxer::new();
        let error = demuxer.push(&packet).unwrap_err().to_string();
        assert!(error.contains("scrambled"), "{error}");
    }
}
