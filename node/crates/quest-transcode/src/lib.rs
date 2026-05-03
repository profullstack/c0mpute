//! FFmpeg-based transcoding worker.
//!
//! We shell out to `ffmpeg` rather than linking it (license hygiene + crash
//! isolation; see PRD §9). This module is responsible for:
//!
//! 1. Detecting available encoders/hwaccels on startup (`probe_capabilities`)
//! 2. Building a command line from a `TranscodeSpec`
//! 3. Running it with a wall-clock budget
//! 4. Returning the resulting bytes for the caller to chunk + announce

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quest_proto::{Codec, HardwarePref, TranscodeSpec};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// What `ffmpeg` reports it can do on this machine.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub hwaccels: Vec<String>,
    pub encoders: Vec<String>,
}

impl Capabilities {
    pub fn supports_encoder(&self, name: &str) -> bool {
        self.encoders.iter().any(|e| e == name)
    }

    pub fn best_hardware_for(&self, codec: Codec) -> HardwarePref {
        let candidates = match codec {
            Codec::H264 | Codec::Hevc => &[
                ("h264_nvenc", HardwarePref::Nvenc),
                ("h264_qsv", HardwarePref::Qsv),
                ("h264_amf", HardwarePref::Amf),
                ("h264_videotoolbox", HardwarePref::VideoToolbox),
            ][..],
            Codec::Av1 => &[
                ("av1_nvenc", HardwarePref::Nvenc),
                ("av1_qsv", HardwarePref::Qsv),
                ("av1_amf", HardwarePref::Amf),
            ][..],
        };
        for (name, pref) in candidates {
            if self.supports_encoder(name) {
                return *pref;
            }
        }
        HardwarePref::Cpu
    }
}

/// Probe the locally installed ffmpeg for hwaccels + encoder list.
pub async fn probe_capabilities(ffmpeg_bin: &Path) -> Result<Capabilities> {
    let hwaccels = run_capture(ffmpeg_bin, &["-hide_banner", "-hwaccels"])
        .await?
        .lines()
        .skip(1) // first line is "Hardware acceleration methods:"
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let encoders_raw = run_capture(ffmpeg_bin, &["-hide_banner", "-encoders"]).await?;
    // Encoder lines look like " V..... h264_nvenc           NVIDIA NVENC H.264..."
    let encoders = encoders_raw
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            if l.starts_with('V') || l.starts_with('A') {
                l.split_whitespace().nth(1).map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    Ok(Capabilities {
        hwaccels,
        encoders,
    })
}

async fn run_capture(bin: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawn {} {:?}", bin.display(), args))?;
    if !out.status.success() {
        bail!(
            "{} {:?} exited {}: {}",
            bin.display(),
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build the FFmpeg command line for a transcode spec.
///
/// Returns a vec of arg strings (excluding the program name itself). The
/// caller is responsible for setting input/output paths.
pub fn build_args(
    input: &Path,
    output: &Path,
    spec: &TranscodeSpec,
    caps: &Capabilities,
) -> Vec<String> {
    let hw = spec
        .hardware_pref
        .unwrap_or_else(|| caps.best_hardware_for(spec.codec));

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-y".into(),
        "-loglevel".into(),
        "warning".into(),
    ];

    // Hardware-accelerated decode where applicable.
    if matches!(hw, HardwarePref::Nvenc) {
        args.extend([
            "-hwaccel".into(),
            "cuda".into(),
            "-hwaccel_output_format".into(),
            "cuda".into(),
        ]);
    } else if matches!(hw, HardwarePref::Qsv) {
        args.extend(["-hwaccel".into(), "qsv".into()]);
    } else if matches!(hw, HardwarePref::VideoToolbox) {
        args.extend(["-hwaccel".into(), "videotoolbox".into()]);
    }

    args.extend(["-i".into(), input.to_string_lossy().into_owned()]);

    let encoder = spec.codec.ffmpeg_encoder(hw);
    args.extend(["-c:v".into(), encoder.into()]);

    // Per-encoder rate-control defaults aligned with PRD §9 examples.
    match (spec.codec, hw) {
        (_, HardwarePref::Nvenc) => {
            args.extend([
                "-preset".into(),
                "p5".into(),
                "-tune".into(),
                "hq".into(),
                "-rc".into(),
                "vbr".into(),
            ]);
        }
        (Codec::Av1, HardwarePref::Cpu) => {
            args.extend([
                "-preset".into(),
                "6".into(),
                "-crf".into(),
                "32".into(),
            ]);
        }
        (_, HardwarePref::Cpu) => {
            args.extend(["-preset".into(), "medium".into()]);
        }
        _ => {}
    }

    args.extend([
        "-b:v".into(),
        format!("{}", spec.bitrate_bps),
        "-maxrate".into(),
        format!("{}", spec.bitrate_bps * 3 / 2),
        "-bufsize".into(),
        format!("{}", spec.bitrate_bps * 2),
        "-g".into(),
        spec.keyframe_interval.to_string(),
        "-vf".into(),
        format!("scale={}:{}", spec.width, spec.height),
    ]);

    args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()]);
    args.extend(spec.extra_ffmpeg_args.iter().cloned());

    args.push(output.to_string_lossy().into_owned());
    args
}

/// Run a transcode from `input_path` to `output_path` using the configured
/// `ffmpeg` binary. Enforces a wall-clock budget.
pub async fn transcode(
    ffmpeg_bin: &Path,
    input_path: &Path,
    output_path: &Path,
    spec: &TranscodeSpec,
    caps: &Capabilities,
    budget: Duration,
) -> Result<PathBuf> {
    let args = build_args(input_path, output_path, spec, caps);
    debug!(?args, "spawning ffmpeg");

    let child = Command::new(ffmpeg_bin)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", ffmpeg_bin.display()))?;

    let pid = child.id();
    info!(pid, "ffmpeg started");

    let result = timeout(budget, child.wait_with_output()).await;

    match result {
        Ok(Ok(out)) if out.status.success() => Ok(output_path.to_path_buf()),
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!(status = ?out.status, stderr = %stderr, "ffmpeg failed");
            bail!("ffmpeg exited {}: {}", out.status, stderr);
        }
        Ok(Err(e)) => Err(e).context("ffmpeg io error"),
        Err(_) => bail!("ffmpeg exceeded wall-clock budget of {:?}", budget),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvenc_args_include_hwaccel() {
        let caps = Capabilities {
            hwaccels: vec!["cuda".into()],
            encoders: vec!["h264_nvenc".into()],
        };
        let spec = TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 5_000_000,
            width: 1920,
            height: 1080,
            keyframe_interval: 60,
            hardware_pref: Some(HardwarePref::Nvenc),
            extra_ffmpeg_args: vec![],
        };
        let args = build_args(
            Path::new("in.ts"),
            Path::new("out.ts"),
            &spec,
            &caps,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-hwaccel cuda"));
        assert!(joined.contains("h264_nvenc"));
        assert!(joined.contains("scale=1920:1080"));
    }

    #[test]
    fn cpu_av1_uses_svt() {
        let caps = Capabilities::default();
        let spec = TranscodeSpec {
            codec: Codec::Av1,
            bitrate_bps: 3_000_000,
            width: 1920,
            height: 1080,
            keyframe_interval: 60,
            hardware_pref: Some(HardwarePref::Cpu),
            extra_ffmpeg_args: vec![],
        };
        let args = build_args(
            Path::new("in.ts"),
            Path::new("out.ts"),
            &spec,
            &caps,
        );
        assert!(args.iter().any(|a| a == "libsvtav1"));
    }
}
