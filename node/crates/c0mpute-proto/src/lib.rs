//! Shared types for everything that crosses a process or network boundary in
//! Quest: chunks, jobs, peer announcements, transcode specs, payouts.
//!
//! These types are mirrored in TypeScript inside `packages/shared` so the
//! coordinator (Bun) and the dashboard (Next.js) talk the same shape.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Length of a blake3 hash in bytes.
pub const HASH_LEN: usize = 32;

/// 32-byte content hash. Display/Debug as lowercase hex; serializes as hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; HASH_LEN]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, ProtoError> {
        let bytes = hex::decode(s).map_err(|_| ProtoError::BadHash)?;
        let arr: [u8; HASH_LEN] = bytes.try_into().map_err(|_| ProtoError::BadHash)?;
        Ok(Self(arr))
    }

    /// Format as the canonical `quest://blake3:<hex>` URL.
    pub fn to_quest_url(&self) -> String {
        format!("quest://blake3:{}", self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("invalid hash encoding")]
    BadHash,
    #[error("unknown codec: {0}")]
    UnknownCodec(String),
}

/// Video codec families supported by Quest workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    /// Best-known FFmpeg encoder name for a hardware preference.
    pub fn ffmpeg_encoder(self, hw: HardwarePref) -> &'static str {
        match (self, hw) {
            (Codec::H264, HardwarePref::Nvenc) => "h264_nvenc",
            (Codec::H264, HardwarePref::Qsv) => "h264_qsv",
            (Codec::H264, HardwarePref::Amf) => "h264_amf",
            (Codec::H264, HardwarePref::VideoToolbox) => "h264_videotoolbox",
            (Codec::H264, HardwarePref::Cpu) => "libx264",

            (Codec::Hevc, HardwarePref::Nvenc) => "hevc_nvenc",
            (Codec::Hevc, HardwarePref::Qsv) => "hevc_qsv",
            (Codec::Hevc, HardwarePref::Amf) => "hevc_amf",
            (Codec::Hevc, HardwarePref::VideoToolbox) => "hevc_videotoolbox",
            (Codec::Hevc, HardwarePref::Cpu) => "libx265",

            (Codec::Av1, HardwarePref::Nvenc) => "av1_nvenc",
            (Codec::Av1, HardwarePref::Qsv) => "av1_qsv",
            (Codec::Av1, HardwarePref::Amf) => "av1_amf",
            (Codec::Av1, HardwarePref::VideoToolbox) => "av1_videotoolbox",
            (Codec::Av1, HardwarePref::Cpu) => "libsvtav1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HardwarePref {
    Nvenc,
    Qsv,
    Amf,
    VideoToolbox,
    Cpu,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscodeSpec {
    pub codec: Codec,
    pub bitrate_bps: u32,
    pub width: u32,
    pub height: u32,
    pub keyframe_interval: u32,
    #[serde(default)]
    pub hardware_pref: Option<HardwarePref>,
    #[serde(default)]
    pub extra_ffmpeg_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscodeJob {
    pub job_id: Uuid,
    pub video_id: Uuid,
    pub rendition_id: Uuid,
    pub input_chunk_hash: Hash,
    pub spec: TranscodeSpec,
    pub deadline_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscodeResult {
    pub job_id: Uuid,
    pub output_chunks: Vec<Hash>,
    pub output_bytes: u64,
    pub duration_seconds: f32,
    pub vmaf_self_score: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkAnnouncement {
    pub chunk_hash: Hash,
    pub shard_index: u8,
    pub bytes: u32,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkRequest {
    pub chunk_hash: Hash,
    pub shard_index: Option<u8>,
}

/// Capability advertisement a worker sends to the coordinator on heartbeat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub roles: Vec<Role>,
    pub codecs_encode: Vec<Codec>,
    pub codecs_decode: Vec<Codec>,
    pub hardware: Vec<HardwarePref>,
    pub free_disk_bytes: u64,
    pub free_vram_bytes: Option<u64>,
    pub region: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Storage,
    Transcode,
    Gateway,
    Verifier,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip_hex() {
        let h = Hash::of(b"hello quest");
        let s = h.to_hex();
        let h2 = Hash::from_hex(&s).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn quest_url_format() {
        let h = Hash::of(b"hello quest");
        assert!(h.to_quest_url().starts_with("quest://blake3:"));
    }

    #[test]
    fn codec_encoder_lookup() {
        assert_eq!(
            Codec::Av1.ffmpeg_encoder(HardwarePref::Nvenc),
            "av1_nvenc"
        );
        assert_eq!(Codec::H264.ffmpeg_encoder(HardwarePref::Cpu), "libx264");
    }
}
