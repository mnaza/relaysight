//! Splitting a TURN byte stream back into messages.
//!
//! Over UDP every datagram is one message. Over TCP or TLS there is no length
//! prefix: a STUN message is its 20-byte header plus the length in that header,
//! and a ChannelData message is its 4-byte header plus its length padded to a
//! multiple of 4 (RFC 8656). The first two bits tell them apart: `00` is STUN,
//! `01` is ChannelData.

use bytes::{Bytes, BytesMut};

const STUN_HEADER_LEN: usize = 20;
const CHANNEL_DATA_HEADER_LEN: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FramingError {
    /// The first two bits are neither STUN (`00`) nor ChannelData (`01`).
    NotTurn(u8),
    /// A STUN length that is not a multiple of 4, which STUN never sends.
    BadStunLength(usize),
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTurn(first) => {
                write!(f, "byte {first:#04x} starts neither STUN nor ChannelData")
            }
            Self::BadStunLength(len) => write!(f, "STUN length {len} is not a multiple of 4"),
        }
    }
}

impl std::error::Error for FramingError {}

#[derive(Debug, Default)]
pub(crate) struct StreamSplitter {
    buf: BytesMut,
}

impl StreamSplitter {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, or `None` until one has arrived.
    pub(crate) fn next_message(&mut self) -> Result<Option<Bytes>, FramingError> {
        if self.buf.len() < CHANNEL_DATA_HEADER_LEN {
            return Ok(None);
        }
        let first = self.buf[0];
        let declared = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
        let total = match first >> 6 {
            0b00 if declared % 4 != 0 => return Err(FramingError::BadStunLength(declared)),
            0b00 => STUN_HEADER_LEN + declared,
            0b01 => CHANNEL_DATA_HEADER_LEN + declared.next_multiple_of(4),
            _ => return Err(FramingError::NotTurn(first)),
        };
        if self.buf.len() < total {
            return Ok(None);
        }
        Ok(Some(self.buf.split_to(total).freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STUN Binding request header followed by `body_len` bytes of body.
    fn stun(body_len: u16) -> Vec<u8> {
        let mut message = vec![0x00, 0x01];
        message.extend_from_slice(&body_len.to_be_bytes());
        message.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        message.extend_from_slice(&[7u8; 12]);
        message.extend(std::iter::repeat_n(0xAB, usize::from(body_len)));
        message
    }

    /// ChannelData on channel 0x4000 carrying `payload`, padded to 4 when `pad`.
    fn channel_data(payload: &[u8], pad: bool) -> Vec<u8> {
        let mut message = vec![0x40, 0x00];
        message.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        message.extend_from_slice(payload);
        while pad && message.len() % 4 != 0 {
            message.push(0);
        }
        message
    }

    #[test]
    fn a_whole_stun_message_comes_out_whole() {
        let message = stun(8);
        let mut splitter = StreamSplitter::default();
        splitter.push(&message);
        assert_eq!(
            splitter.next_message().unwrap().as_deref(),
            Some(&message[..])
        );
        assert_eq!(splitter.next_message().unwrap(), None);
    }

    #[test]
    fn channel_data_comes_out_with_its_padding() {
        let message = channel_data(&[1, 2, 3, 4, 5], true);
        assert_eq!(message.len(), 12, "4 header + 5 payload + 3 padding");
        let mut splitter = StreamSplitter::default();
        splitter.push(&message);
        assert_eq!(
            splitter.next_message().unwrap().as_deref(),
            Some(&message[..])
        );
    }

    #[test]
    fn unpadded_channel_data_waits_for_its_padding() {
        // Over a stream the sender must pad, so the message is incomplete until it does.
        let mut splitter = StreamSplitter::default();
        splitter.push(&channel_data(&[1, 2, 3, 4, 5], false));
        assert_eq!(splitter.next_message().unwrap(), None);
        splitter.push(&[0, 0, 0]);
        assert_eq!(splitter.next_message().unwrap().map(|m| m.len()), Some(12));
    }

    #[test]
    fn two_messages_in_one_read_come_out_as_two() {
        let first = stun(4);
        let second = channel_data(&[9; 6], true);
        let mut splitter = StreamSplitter::default();
        splitter.push(&[first.clone(), second.clone()].concat());
        assert_eq!(
            splitter.next_message().unwrap().as_deref(),
            Some(&first[..])
        );
        assert_eq!(
            splitter.next_message().unwrap().as_deref(),
            Some(&second[..])
        );
        assert_eq!(splitter.next_message().unwrap(), None);
    }

    #[test]
    fn a_message_split_across_reads_comes_out_once_complete() {
        let message = stun(12);
        let mut splitter = StreamSplitter::default();
        splitter.push(&message[..3]);
        assert_eq!(
            splitter.next_message().unwrap(),
            None,
            "not even a header yet"
        );
        splitter.push(&message[3..25]);
        assert_eq!(
            splitter.next_message().unwrap(),
            None,
            "header but not the whole body"
        );
        splitter.push(&message[25..]);
        assert_eq!(
            splitter.next_message().unwrap().as_deref(),
            Some(&message[..])
        );
    }

    #[test]
    fn bytes_that_are_neither_stun_nor_channel_data_are_refused() {
        let mut splitter = StreamSplitter::default();
        splitter.push(&[0x80, 0x00, 0x00, 0x00]);
        assert_eq!(splitter.next_message(), Err(FramingError::NotTurn(0x80)));
    }

    #[test]
    fn a_stun_length_that_is_not_a_multiple_of_four_is_refused() {
        let mut splitter = StreamSplitter::default();
        splitter.push(&stun(5));
        assert_eq!(splitter.next_message(), Err(FramingError::BadStunLength(5)));
    }
}
