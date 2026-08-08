//! DDP (Distributed Display Protocol) framing and transport.
//!
//! Encoding follows WLED's own receiver and sender rather than a paraphrase of the
//! 3waylabs specification, because WLED is the only implementation this server has to
//! satisfy. The constants and field widths below correspond to `DDP_*` in WLED's
//! `wled00/src/dependencies/e131/ESPAsyncE131.h` and the sender in `wled00/udp.cpp`.

use matrix_frame::Frame;
use std::net::SocketAddr;
use thiserror::Error;
use tokio::net::UdpSocket;

/// WLED listens for DDP here and identifies a packet as DDP by this port alone.
pub const DDP_PORT: u16 = 4048;

const HEADER_LEN: usize = 10;

/// Payload bytes per packet. WLED's own sender uses this figure (480 RGB pixels), and
/// it keeps a packet inside a single 1500-byte MTU with the header and UDP/IP overhead.
const CHANNELS_PER_PACKET: usize = 1440;

const FLAGS_VER1: u8 = 0x40;
const FLAGS_PUSH: u8 = 0x01;

/// RGB, 8 bits per channel, 3 channels. A HUB75 panel has no white channel.
const TYPE_RGB24: u8 = 0x0B;

/// Default output device. WLED rejects the control, status, and config destinations.
const ID_DISPLAY: u8 = 1;

#[derive(Debug, Error)]
pub enum DdpError {
    #[error("failed to bind a local UDP socket: {0}")]
    Bind(#[source] std::io::Error),

    #[error("failed to send a DDP packet to {addr}: {source}")]
    Send {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("sent {sent} of {expected} bytes in a DDP packet to {addr}")]
    ShortWrite {
        addr: SocketAddr,
        sent: usize,
        expected: usize,
    },
}

impl DdpError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Bind(_) => "ddp_bind_failed",
            Self::Send { .. } => "ddp_send_failed",
            Self::ShortWrite { .. } => "ddp_short_write",
        }
    }
}

/// Sequence number carried in every packet.
///
/// WLED treats 0 as "unused" and only tracks 1-15, so this wraps within that range
/// rather than through zero. Out-of-sequence rejection is opt-in on the device, but a
/// correct sequence costs nothing and is what lets it work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence(u8);

impl Sequence {
    pub fn new() -> Self {
        Self(1)
    }

    fn next(&mut self) -> u8 {
        let current = self.0;
        self.0 = if current >= 15 { 1 } else { current + 1 };
        current
    }
}

impl Default for Sequence {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a frame into the DDP packets that carry it.
///
/// The final packet carries the PUSH flag, which is what tells WLED to render. Every
/// earlier packet is buffered by the device until that arrives, so a frame is atomic
/// on the panel even though it crosses several datagrams.
pub fn frame_packets(frame: &Frame, sequence: &mut Sequence) -> Vec<Vec<u8>> {
    let payload = frame.as_rgb();
    let chunks: Vec<&[u8]> = if payload.is_empty() {
        Vec::new()
    } else {
        payload.chunks(CHANNELS_PER_PACKET).collect()
    };

    let last = chunks.len().saturating_sub(1);
    let mut offset: u32 = 0;
    let mut packets = Vec::with_capacity(chunks.len());

    for (index, chunk) in chunks.iter().enumerate() {
        let mut flags = FLAGS_VER1;
        if index == last {
            flags |= FLAGS_PUSH;
        }

        let mut packet = Vec::with_capacity(HEADER_LEN + chunk.len());
        packet.push(flags);
        packet.push(sequence.next() & 0x0F);
        packet.push(TYPE_RGB24);
        packet.push(ID_DISPLAY);
        packet.extend_from_slice(&offset.to_be_bytes());
        packet.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        packet.extend_from_slice(chunk);

        packets.push(packet);
        offset += chunk.len() as u32;
    }

    packets
}

/// A bound UDP socket pointed at one panel.
#[derive(Debug)]
pub struct DdpSender {
    socket: UdpSocket,
    target: SocketAddr,
    sequence: Sequence,
}

impl DdpSender {
    pub async fn connect(target: SocketAddr) -> Result<Self, DdpError> {
        let bind: SocketAddr = if target.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid bind address")
        } else {
            "[::]:0".parse().expect("valid bind address")
        };
        let socket = UdpSocket::bind(bind).await.map_err(DdpError::Bind)?;
        Ok(Self {
            socket,
            target,
            sequence: Sequence::new(),
        })
    }

    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Send one frame.
    ///
    /// Datagrams are sent back to back with no pacing. A dropped packet loses that
    /// region of one frame and the next frame overwrites it, which is the right
    /// trade for a display: retransmitting stale pixels is worse than showing the
    /// next ones.
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), DdpError> {
        for packet in frame_packets(frame, &mut self.sequence) {
            let sent = self
                .socket
                .send_to(&packet, self.target)
                .await
                .map_err(|source| DdpError::Send {
                    addr: self.target,
                    source,
                })?;
            if sent != packet.len() {
                return Err(DdpError::ShortWrite {
                    addr: self.target,
                    sent,
                    expected: packet.len(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_frame::{Canvas, Rgb};

    fn m1() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn parse_header(packet: &[u8]) -> (u8, u8, u8, u8, u32, u16) {
        (
            packet[0],
            packet[1],
            packet[2],
            packet[3],
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            u16::from_be_bytes([packet[8], packet[9]]),
        )
    }

    #[test]
    fn a_full_m1_frame_is_nine_packets() {
        let frame = Frame::blank(m1());
        let packets = frame_packets(&frame, &mut Sequence::new());
        // 12288 bytes / 1440 per packet = 8 full + 1 remainder.
        assert_eq!(packets.len(), 9);
    }

    #[test]
    fn offsets_are_byte_counts_and_tile_the_payload_exactly() {
        let frame = Frame::blank(m1());
        let packets = frame_packets(&frame, &mut Sequence::new());

        let mut expected_offset = 0u32;
        let mut total_payload = 0usize;
        for packet in &packets {
            let (_, _, _, _, offset, len) = parse_header(packet);
            assert_eq!(offset, expected_offset);
            assert_eq!(usize::from(len), packet.len() - HEADER_LEN);
            expected_offset += u32::from(len);
            total_payload += usize::from(len);
        }
        assert_eq!(total_payload, frame.as_rgb().len());
    }

    #[test]
    fn only_the_last_packet_carries_push() {
        let frame = Frame::blank(m1());
        let packets = frame_packets(&frame, &mut Sequence::new());

        for packet in &packets[..packets.len() - 1] {
            assert_eq!(packet[0] & FLAGS_PUSH, 0, "only the last packet pushes");
            assert_eq!(packet[0] & 0xC0, FLAGS_VER1, "version bits must be v1");
        }
        let last = packets.last().expect("at least one packet");
        assert_eq!(last[0] & FLAGS_PUSH, FLAGS_PUSH);
        assert_eq!(last[0] & 0xC0, FLAGS_VER1);
    }

    #[test]
    fn every_packet_declares_rgb24_to_the_display_output() {
        let frame = Frame::blank(m1());
        for packet in frame_packets(&frame, &mut Sequence::new()) {
            let (_, _, data_type, destination, _, _) = parse_header(&packet);
            assert_eq!(data_type, TYPE_RGB24);
            assert_eq!(destination, ID_DISPLAY);
        }
    }

    #[test]
    fn no_packet_payload_exceeds_the_wled_limit() {
        let frame = Frame::blank(m1());
        for packet in frame_packets(&frame, &mut Sequence::new()) {
            assert!(packet.len() - HEADER_LEN <= CHANNELS_PER_PACKET);
            // Header plus payload has to clear a 1500-byte MTU with room for UDP/IP.
            assert!(packet.len() <= 1458);
        }
    }

    #[test]
    fn pixel_bytes_survive_framing_in_order() {
        let mut frame = Frame::blank(m1());
        frame.set(0, 0, Rgb::new(1, 2, 3));
        frame.set(63, 63, Rgb::new(7, 8, 9));

        let packets = frame_packets(&frame, &mut Sequence::new());
        let reassembled: Vec<u8> = packets
            .iter()
            .flat_map(|p| p[HEADER_LEN..].to_vec())
            .collect();

        assert_eq!(reassembled, frame.as_rgb());
        assert_eq!(&reassembled[0..3], &[1, 2, 3]);
        assert_eq!(&reassembled[12_285..12_288], &[7, 8, 9]);
    }

    #[test]
    fn sequence_wraps_within_one_to_fifteen_and_never_emits_zero() {
        let mut sequence = Sequence::new();
        let emitted: Vec<u8> = (0..40).map(|_| sequence.next()).collect();
        assert!(emitted.iter().all(|&s| (1..=15).contains(&s)));
        assert_eq!(
            &emitted[..16],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 1]
        );
    }

    #[test]
    fn sequence_field_stays_inside_four_bits_on_the_wire() {
        let frame = Frame::blank(Canvas::new(4, 1).expect("valid"));
        let mut sequence = Sequence::new();
        for _ in 0..40 {
            for packet in frame_packets(&frame, &mut sequence) {
                assert_eq!(packet[1] & 0xF0, 0, "sequence must fit the low nibble");
            }
        }
    }

    #[test]
    fn a_frame_smaller_than_one_packet_is_a_single_pushed_packet() {
        let frame = Frame::blank(Canvas::new(8, 8).expect("valid"));
        let packets = frame_packets(&frame, &mut Sequence::new());
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][0] & FLAGS_PUSH, FLAGS_PUSH);
        let (_, _, _, _, offset, len) = parse_header(&packets[0]);
        assert_eq!(offset, 0);
        assert_eq!(usize::from(len), 8 * 8 * 3);
    }

    #[tokio::test]
    async fn send_frame_delivers_every_packet_to_a_loopback_listener() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind listener");
        let target = listener.local_addr().expect("listener addr");

        let mut sender = DdpSender::connect(target).await.expect("connect");
        // A varied frame: an all-black payload would compare equal to a zeroed buffer
        // even if nothing was copied into it.
        let mut frame = Frame::blank(m1());
        for y in 0..64u16 {
            for x in 0..64u16 {
                frame.set(x, y, Rgb::new(x as u8, y as u8, (x ^ y) as u8));
            }
        }
        sender.send_frame(&frame).await.expect("send");

        // Reassemble by declared offset rather than arrival order: UDP does not promise
        // ordering, and the offset field is what the receiver actually keys on.
        let mut reassembled = vec![0u8; frame.as_rgb().len()];
        let mut covered = 0usize;
        let mut buf = vec![0u8; 2048];
        for _ in 0..9 {
            let (n, _) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                listener.recv_from(&mut buf),
            )
            .await
            .expect("packet within timeout")
            .expect("recv");

            let (_, _, _, _, offset, len) = parse_header(&buf[..n]);
            let offset = offset as usize;
            let len = usize::from(len);
            assert_eq!(
                len,
                n - HEADER_LEN,
                "declared length must match the datagram"
            );
            reassembled[offset..offset + len].copy_from_slice(&buf[HEADER_LEN..n]);
            covered += len;
        }

        assert_eq!(covered, frame.as_rgb().len());
        assert_eq!(
            reassembled,
            frame.as_rgb(),
            "what arrived on the wire must equal the frame byte for byte"
        );
    }
}
