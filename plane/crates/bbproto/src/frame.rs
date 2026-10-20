use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const MAGIC: u16 = 0xBB01;
pub const HEADER_LEN: usize = 8;
/// Largest payload either side accepts.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
/// Largest frame payload in one UDP datagram (the stock feed uses <= 1440-byte datagrams).
pub const MAX_DATAGRAM_PAYLOAD: usize = 1400;
/// Microphone samples per datagram (32 ms).
pub const MIC_SAMPLES_PER_DATAGRAM: usize = 512;

/// Frame flag: a `Video` frame that answers a `snapshot` request.
/// Its `seq` field then carries the request id.
pub const FLAG_SNAPSHOT: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// JSON control message, both directions.
    Ctrl = 1,
    /// Robot -> plane: `MediaHeader` + `w u16, h u16` + JPEG.
    Video = 2,
    /// Robot -> plane: `MediaHeader` + PCM s16le 16 kHz mono.
    MicPcm = 3,
    /// Plane -> robot: PCM s16le 16 kHz mono, no header.
    SpkPcm = 4,
}

impl TryFrom<u8> for FrameType {
    type Error = FrameError;
    fn try_from(v: u8) -> Result<Self, FrameError> {
        Ok(match v {
            1 => Self::Ctrl,
            2 => Self::Video,
            3 => Self::MicPcm,
            4 => Self::SpkPcm,
            _ => return Err(FrameError::UnknownType(v)),
        })
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("bad magic {0:#06x}")]
    BadMagic(u16),
    #[error("unknown frame type {0}")]
    UnknownType(u8),
    #[error("payload too large: {0} bytes")]
    TooLarge(usize),
    #[error("payload too short for {0:?}")]
    Short(FrameType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: FrameType,
    pub flags: u8,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(ty: FrameType, payload: impl Into<Bytes>) -> Self {
        Self {
            ty,
            flags: 0,
            payload: payload.into(),
        }
    }

    pub fn ctrl(msg: &crate::ToRobot) -> Self {
        Self::new(
            FrameType::Ctrl,
            serde_json::to_vec(msg).expect("ToRobot serializes"),
        )
    }

    pub fn encode(&self, out: &mut BytesMut) {
        out.reserve(HEADER_LEN + self.payload.len());
        out.put_u16_le(MAGIC);
        out.put_u8(self.ty as u8);
        out.put_u8(self.flags);
        out.put_u32_le(self.payload.len() as u32);
        out.put_slice(&self.payload);
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::new();
        self.encode(&mut b);
        b.freeze()
    }

    /// Decode one frame from the front of `buf`. Returns `Ok(None)` if more bytes are needed.
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != MAGIC {
            return Err(FrameError::BadMagic(magic));
        }
        let ty = FrameType::try_from(buf[2])?;
        let flags = buf[3];
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(FrameError::TooLarge(len));
        }
        if buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        buf.advance(HEADER_LEN);
        let payload = buf.split_to(len).freeze();
        Ok(Some(Frame { ty, flags, payload }))
    }
}

/// Header in front of `Video` and `MicPcm` payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    /// Frame/chunk sequence number, or the request id for a snapshot.
    pub seq: u32,
    /// Robot uptime in milliseconds when captured.
    pub ts_ms: u32,
}

impl MediaHeader {
    pub const LEN: usize = 8;

    pub fn parse(p: &[u8]) -> Option<(MediaHeader, &[u8])> {
        if p.len() < Self::LEN {
            return None;
        }
        let seq = u32::from_le_bytes(p[0..4].try_into().ok()?);
        let ts_ms = u32::from_le_bytes(p[4..8].try_into().ok()?);
        Some((MediaHeader { seq, ts_ms }, &p[Self::LEN..]))
    }

    pub fn put(&self, out: &mut BytesMut) {
        out.put_u32_le(self.seq);
        out.put_u32_le(self.ts_ms);
    }
}

/// A decoded `Video` frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub header: MediaHeader,
    pub snapshot: bool,
    pub width: u16,
    pub height: u16,
    pub jpeg: Bytes,
}

impl VideoFrame {
    /// Split into `Video` datagram frames (each repeats seq/ts/w/h plus frag/frags).
    pub fn to_datagrams(&self) -> Vec<Frame> {
        let chunk = MAX_DATAGRAM_PAYLOAD - VideoFragment::HEADER_LEN;
        let frags = self.jpeg.len().div_ceil(chunk).max(1);
        (0..frags)
            .map(|i| {
                let part = &self.jpeg[i * chunk..((i + 1) * chunk).min(self.jpeg.len())];
                let mut b = BytesMut::with_capacity(VideoFragment::HEADER_LEN + part.len());
                self.header.put(&mut b);
                b.put_u16_le(self.width);
                b.put_u16_le(self.height);
                b.put_u16_le(i as u16);
                b.put_u16_le(frags as u16);
                b.put_slice(part);
                Frame {
                    ty: FrameType::Video,
                    flags: if self.snapshot { FLAG_SNAPSHOT } else { 0 },
                    payload: b.freeze(),
                }
            })
            .collect()
    }
}

/// One `Video` datagram: part `frag` of `frags` of a JPEG frame.
#[derive(Debug, Clone)]
pub struct VideoFragment {
    pub header: MediaHeader,
    pub snapshot: bool,
    pub width: u16,
    pub height: u16,
    pub frag: u16,
    pub frags: u16,
    pub data: Bytes,
}

impl VideoFragment {
    pub const HEADER_LEN: usize = MediaHeader::LEN + 8;

    pub fn parse(frame: &Frame) -> Result<VideoFragment, FrameError> {
        let p = &frame.payload;
        let (header, rest) = MediaHeader::parse(p).ok_or(FrameError::Short(FrameType::Video))?;
        if rest.len() < 8 {
            return Err(FrameError::Short(FrameType::Video));
        }
        let u = |i: usize| u16::from_le_bytes([rest[i], rest[i + 1]]);
        let (frag, frags) = (u(4), u(6));
        if frags == 0 || frag >= frags {
            return Err(FrameError::Short(FrameType::Video));
        }
        Ok(VideoFragment {
            header,
            snapshot: frame.flags & FLAG_SNAPSHOT != 0,
            width: u(0),
            height: u(2),
            frag,
            frags,
            data: frame.payload.slice(Self::HEADER_LEN..),
        })
    }
}

/// Reassembles fragmented video frames; incomplete frames are dropped, never waited for.
#[derive(Default)]
pub struct VideoAssembler {
    partial: std::collections::HashMap<(u32, bool), Partial>,
    /// Frames given up on because some of their fragments never arrived.
    pub dropped: u64,
}

struct Partial {
    parts: Vec<Option<Bytes>>,
    have: usize,
    first: MediaHeader,
    width: u16,
    height: u16,
}

impl VideoAssembler {
    /// Add a fragment; returns the frame once all of its parts have arrived.
    pub fn push(&mut self, f: VideoFragment) -> Option<VideoFrame> {
        let key = (f.header.seq, f.snapshot);
        // Live frames are numbered upwards: anything older than this one is stale.
        if !f.snapshot {
            let before = self.partial.len();
            self.partial
                .retain(|(seq, snap), _| *snap || seq.wrapping_sub(f.header.seq) as i32 >= -8);
            self.dropped += (before - self.partial.len()) as u64;
        }
        if self.partial.len() > 16 {
            self.dropped += self.partial.len() as u64;
            self.partial.clear();
        }
        let p = self.partial.entry(key).or_insert_with(|| Partial {
            parts: vec![None; f.frags as usize],
            have: 0,
            first: f.header,
            width: f.width,
            height: f.height,
        });
        if p.parts.len() != f.frags as usize {
            return None;
        }
        let slot = &mut p.parts[f.frag as usize];
        if slot.is_none() {
            *slot = Some(f.data);
            p.have += 1;
        }
        if p.have < p.parts.len() {
            return None;
        }
        let p = self.partial.remove(&key)?;
        let mut jpeg = BytesMut::new();
        for part in p.parts.into_iter().flatten() {
            jpeg.put_slice(&part);
        }
        Some(VideoFrame {
            header: p.first,
            snapshot: key.1,
            width: p.width,
            height: p.height,
            jpeg: jpeg.freeze(),
        })
    }
}

/// A decoded `MicPcm` frame.
#[derive(Debug, Clone)]
pub struct MicChunk {
    pub header: MediaHeader,
    pub samples: Vec<i16>,
}

impl MicChunk {
    pub fn parse(frame: &Frame) -> Result<MicChunk, FrameError> {
        let (header, rest) =
            MediaHeader::parse(&frame.payload).ok_or(FrameError::Short(FrameType::MicPcm))?;
        Ok(MicChunk {
            header,
            samples: pcm_from_le(rest),
        })
    }

    pub fn to_frame(&self) -> Frame {
        let mut b = BytesMut::with_capacity(MediaHeader::LEN + self.samples.len() * 2);
        self.header.put(&mut b);
        for s in &self.samples {
            b.put_i16_le(*s);
        }
        Frame::new(FrameType::MicPcm, b.freeze())
    }
}

pub fn pcm_from_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| i16::from_le_bytes(c))
        .collect()
}

pub fn pcm_to_le(samples: &[i16]) -> Bytes {
    let mut b = BytesMut::with_capacity(samples.len() * 2);
    for s in samples {
        b.put_i16_le(*s);
    }
    b.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_is_stable() {
        let f = Frame {
            ty: FrameType::Ctrl,
            flags: 0x01,
            payload: Bytes::from_static(b"{}"),
        };
        assert_eq!(
            &f.to_bytes()[..],
            &[0x01, 0xBB, 1, 1, 2, 0, 0, 0, b'{', b'}']
        );
    }

    #[test]
    fn roundtrip_partial_reads() {
        let a = Frame::new(FrameType::SpkPcm, pcm_to_le(&[1, -2, 3]));
        let b = Frame::new(FrameType::Ctrl, Bytes::from_static(b"{}"));
        let mut wire = BytesMut::new();
        a.encode(&mut wire);
        b.encode(&mut wire);
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        for byte in wire {
            buf.put_u8(byte);
            while let Some(f) = Frame::decode(&mut buf).unwrap() {
                out.push(f);
            }
        }
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn video_fragments_reassemble_and_drop_incomplete() {
        let jpeg: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let v = VideoFrame {
            header: MediaHeader { seq: 7, ts_ms: 99 },
            snapshot: false,
            width: 128,
            height: 128,
            jpeg: Bytes::from(jpeg.clone()),
        };
        let dgrams = v.to_datagrams();
        assert_eq!(dgrams.len(), 3);
        assert!(
            dgrams
                .iter()
                .all(|f| f.payload.len() <= MAX_DATAGRAM_PAYLOAD)
        );

        // Out of order, with a duplicate: completes once.
        let mut asm = VideoAssembler::default();
        let frag = |i: usize| VideoFragment::parse(&dgrams[i]).unwrap();
        assert!(asm.push(frag(2)).is_none());
        assert!(asm.push(frag(0)).is_none());
        assert!(asm.push(frag(0)).is_none());
        let out = asm.push(frag(1)).expect("complete");
        assert_eq!(&out.jpeg[..], &jpeg[..]);
        assert_eq!((out.width, out.height, out.header.seq), (128, 128, 7));

        // A lost fragment means no frame, and later frames are unaffected.
        let mut v2 = v.clone();
        v2.header.seq = 8;
        let d2 = v2.to_datagrams();
        asm.push(VideoFragment::parse(&d2[0]).unwrap());
        let mut v3 = v.clone();
        v3.header.seq = 9;
        v3.snapshot = true;
        let mut got = None;
        for f in v3.to_datagrams() {
            got = asm.push(VideoFragment::parse(&f).unwrap()).or(got);
        }
        assert!(got.unwrap().snapshot);
    }

    #[test]
    fn rejects_garbage() {
        let mut buf = BytesMut::from(&[0u8, 0, 1, 0, 0, 0, 0, 0][..]);
        assert!(matches!(
            Frame::decode(&mut buf),
            Err(FrameError::BadMagic(0))
        ));
        let mut buf = BytesMut::new();
        buf.put_u16_le(MAGIC);
        buf.put_u8(1);
        buf.put_u8(0);
        buf.put_u32_le((MAX_PAYLOAD + 1) as u32);
        assert!(matches!(
            Frame::decode(&mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn mic_chunk_roundtrip() {
        let c = MicChunk {
            header: MediaHeader { seq: 1, ts_ms: 2 },
            samples: vec![i16::MIN, 0, i16::MAX],
        };
        let back = MicChunk::parse(&c.to_frame()).unwrap();
        assert_eq!(back.samples, c.samples);
        assert_eq!(back.header, c.header);
    }
}
