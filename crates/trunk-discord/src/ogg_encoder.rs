use std::io::Cursor;

use anyhow::Result;
use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};
use ogg::writing::{PacketWriteEndInfo, PacketWriter};

/// 20ms frame at 16 kHz = 320 samples.
const FRAME_SIZE: usize = 320;

/// Each 320-sample frame at 16 kHz equals 960 granule units (48 kHz basis per RFC 7845).
const GRANULE_PER_FRAME: u64 = 960;

/// Encoded Opus frame with its granule position.
struct OpusFrame {
    data: Vec<u8>,
    granule_pos: u64,
}

/// Streaming OGG Opus encoder for 16 kHz mono audio.
///
/// Call [`process`] with f32 samples as they arrive, then [`finish`] to
/// obtain the complete OGG file bytes. Opus frames are encoded eagerly;
/// the OGG container is written at [`finish`] time to avoid lifetime issues
/// with the borrowed [`PacketWriter`].
pub struct OggOpusEncoder {
    encoder: Encoder,
    pre_skip: u16,
    granule_pos: u64,
    frames: Vec<OpusFrame>,
    pending: Vec<f32>,
    encode_buf: Vec<u8>,
}

impl OggOpusEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz16000, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(16_000))?;

        let lookahead = encoder.lookahead()?;
        // Pre-skip is at 48 kHz; input is 16 kHz, so multiply by 3.
        let pre_skip = u16::try_from(
            lookahead
                .checked_mul(3)
                .ok_or_else(|| anyhow::anyhow!("lookahead overflow: {lookahead}"))?,
        )
        .map_err(|_| anyhow::anyhow!("pre-skip overflow: {lookahead}"))?;

        Ok(Self {
            encoder,
            pre_skip,
            granule_pos: 0,
            frames: Vec::new(),
            pending: Vec::with_capacity(FRAME_SIZE),
            encode_buf: vec![0u8; 1024],
        })
    }

    /// Feed processed 16 kHz mono f32 samples. Complete frames are Opus-encoded
    /// immediately; leftover samples are buffered until the next call.
    pub fn process(&mut self, samples: &[f32]) -> Result<()> {
        self.pending.extend_from_slice(samples);

        while self.pending.len() >= FRAME_SIZE {
            let frame: Vec<f32> = self.pending.drain(..FRAME_SIZE).collect();
            let len = self.encoder.encode_float(&frame, &mut self.encode_buf)?;

            self.granule_pos += GRANULE_PER_FRAME;
            self.frames.push(OpusFrame {
                data: self.encode_buf[..len].to_vec(),
                granule_pos: self.granule_pos,
            });
        }

        Ok(())
    }

    /// Flush remaining samples (zero-padded to a full frame), wrap all encoded
    /// frames in an OGG container, and return the complete file bytes.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        // Pad to a full frame (or create a silence frame for the EndStream marker).
        self.pending.resize(FRAME_SIZE, 0.0);
        let len = self
            .encoder
            .encode_float(&self.pending, &mut self.encode_buf)?;
        self.granule_pos += GRANULE_PER_FRAME;
        self.frames.push(OpusFrame {
            data: self.encode_buf[..len].to_vec(),
            granule_pos: self.granule_pos,
        });

        // Build OGG container around the encoded frames.
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = PacketWriter::new(&mut cursor);
            let serial = 1;

            writer.write_packet(
                build_opus_head(self.pre_skip),
                serial,
                PacketWriteEndInfo::EndPage,
                0,
            )?;
            writer.write_packet(build_opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;

            let last_idx = self.frames.len() - 1;
            for (i, frame) in self.frames.into_iter().enumerate() {
                let end_info = if i == last_idx {
                    PacketWriteEndInfo::EndStream
                } else {
                    PacketWriteEndInfo::NormalPacket
                };
                writer.write_packet(frame.data, serial, end_info, frame.granule_pos)?;
            }
        }

        Ok(cursor.into_inner())
    }
}

/// RFC 7845 Section 5.1 — `OpusHead` identification header (19 bytes for mono).
fn build_opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead"); // magic
    head.push(1); // version
    head.push(1); // channel count (mono)
    head.extend_from_slice(&pre_skip.to_le_bytes()); // pre-skip (48 kHz samples)
    head.extend_from_slice(&16000_u32.to_le_bytes()); // input sample rate
    head.extend_from_slice(&0_i16.to_le_bytes()); // output gain (dB, Q7.8)
    head.push(0); // channel mapping family 0
    head
}

/// RFC 7845 Section 5.2 — `OpusTags` comment header (minimal, empty).
fn build_opus_tags() -> Vec<u8> {
    let mut tags = Vec::with_capacity(16);
    tags.extend_from_slice(b"OpusTags"); // magic
    tags.extend_from_slice(&0_u32.to_le_bytes()); // vendor string length
    tags.extend_from_slice(&0_u32.to_le_bytes()); // user comment list length
    tags
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use ogg::reading::PacketReader;

    use super::*;

    fn sine_wave(freq: f32, duration_s: f32) -> Vec<f32> {
        let n = (16000.0 * duration_s) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 16000.0;
                0.5 * (2.0 * PI * freq * t).sin()
            })
            .collect()
    }

    #[test]
    fn round_trip_produces_valid_ogg() {
        let mut enc = OggOpusEncoder::new().unwrap();
        let audio = sine_wave(1000.0, 1.0); // 1 second of 1 kHz tone
        enc.process(&audio).unwrap();
        let ogg_data = enc.finish().unwrap();

        // Verify OGG container is parseable and has the right structure.
        let mut reader = PacketReader::new(Cursor::new(&ogg_data));

        // First packet: OpusHead
        let head = reader.read_packet().unwrap().unwrap();
        assert!(head.data.starts_with(b"OpusHead"), "missing OpusHead magic");
        assert_eq!(head.data[8], 1, "version should be 1");
        assert_eq!(head.data[9], 1, "channel count should be 1");
        let input_rate = u32::from_le_bytes(head.data[12..16].try_into().unwrap());
        assert_eq!(input_rate, 16000);

        // Second packet: OpusTags
        let tags = reader.read_packet().unwrap().unwrap();
        assert!(tags.data.starts_with(b"OpusTags"), "missing OpusTags magic");

        // Remaining packets: encoded audio
        let mut audio_packets = 0;
        let mut last_was_end = false;
        while let Some(pkt) = reader.read_packet().unwrap() {
            audio_packets += 1;
            last_was_end = pkt.last_in_stream();
        }
        assert!(audio_packets > 0, "expected at least one audio packet");
        assert!(last_was_end, "last packet should have end-of-stream flag");
    }

    #[test]
    fn incremental_process_matches_bulk() {
        // Feed audio in small irregular chunks and verify it still produces valid OGG.
        let audio = sine_wave(440.0, 0.5);
        let mut enc = OggOpusEncoder::new().unwrap();
        for chunk in audio.chunks(100) {
            enc.process(chunk).unwrap();
        }
        let ogg_data = enc.finish().unwrap();

        let mut reader = PacketReader::new(Cursor::new(&ogg_data));
        let head = reader.read_packet().unwrap().unwrap();
        assert!(head.data.starts_with(b"OpusHead"));

        let tags = reader.read_packet().unwrap().unwrap();
        assert!(tags.data.starts_with(b"OpusTags"));

        // Drain remaining packets, verify granule positions are monotonically increasing.
        let mut prev_granule = 0;
        while let Some(pkt) = reader.read_packet().unwrap() {
            assert!(
                pkt.absgp_page() >= prev_granule,
                "granule positions should be monotonically increasing"
            );
            prev_granule = pkt.absgp_page();
        }
    }

    #[test]
    fn empty_audio_produces_valid_ogg() {
        // No audio processed at all — should still produce a valid file
        // with headers and one silence frame.
        let enc = OggOpusEncoder::new().unwrap();
        let ogg_data = enc.finish().unwrap();

        let mut reader = PacketReader::new(Cursor::new(&ogg_data));
        assert!(
            reader
                .read_packet()
                .unwrap()
                .unwrap()
                .data
                .starts_with(b"OpusHead")
        );
        assert!(
            reader
                .read_packet()
                .unwrap()
                .unwrap()
                .data
                .starts_with(b"OpusTags")
        );

        let audio_pkt = reader.read_packet().unwrap().unwrap();
        assert!(audio_pkt.last_in_stream());
    }
}
