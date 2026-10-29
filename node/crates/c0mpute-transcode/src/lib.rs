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
use c0mpute_proto::{Codec, HardwarePref, TranscodeSpec};
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
            Codec::H264 => &[
                ("h264_nvenc", HardwarePref::Nvenc),
                ("h264_qsv", HardwarePref::Qsv),
                ("h264_amf", HardwarePref::Amf),
                ("h264_videotoolbox", HardwarePref::VideoToolbox),
            ][..],
            Codec::Hevc => &[
                ("hevc_nvenc", HardwarePref::Nvenc),
                ("hevc_qsv", HardwarePref::Qsv),
                ("hevc_amf", HardwarePref::Amf),
                ("hevc_videotoolbox", HardwarePref::VideoToolbox),
            ][..],
            Codec::Av1 => &[
                ("av1_nvenc", HardwarePref::Nvenc),
                ("av1_qsv", HardwarePref::Qsv),
                ("av1_amf", HardwarePref::Amf),
                ("av1_videotoolbox", HardwarePref::VideoToolbox),
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
    let mut encoders: Vec<String> = encoders_raw
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

    // `ffmpeg -encoders` reports what the binary was built with, not what
    // this host can actually run. A CPU-only machine can list NVENC and CUDA,
    // then fail default jobs when the driver/device is absent. Keep only
    // hardware encoders that pass a tiny real encode on this host.
    const HARDWARE_ENCODERS: &[&str] = &[
        "h264_nvenc",
        "hevc_nvenc",
        "av1_nvenc",
        "h264_qsv",
        "hevc_qsv",
        "av1_qsv",
        "h264_amf",
        "hevc_amf",
        "av1_amf",
        "h264_videotoolbox",
        "hevc_videotoolbox",
        "av1_videotoolbox",
    ];
    for encoder in HARDWARE_ENCODERS {
        if encoders.iter().any(|e| e == encoder) && !probe_encoder(ffmpeg_bin, encoder).await {
            encoders.retain(|e| e != encoder);
        }
    }

    Ok(Capabilities { hwaccels, encoders })
}

async fn probe_encoder(ffmpeg_bin: &Path, encoder: &str) -> bool {
    let result = timeout(
        Duration::from_secs(5),
        Command::new(ffmpeg_bin)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=1",
                "-frames:v",
                "1",
                "-an",
                "-c:v",
                encoder,
                "-f",
                "null",
                "-",
            ])
            .output(),
    )
    .await;
    matches!(result, Ok(Ok(output)) if output.status.success())
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

    // Decode on CPU for broad input compatibility. Hardware encoding still
    // accelerates output without requiring a matching hardware decoder.
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
            args.extend(["-preset".into(), "6".into(), "-crf".into(), "32".into()]);
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
    let hardware = spec
        .hardware_pref
        .unwrap_or_else(|| caps.best_hardware_for(spec.codec));
    let encoder = spec.codec.ffmpeg_encoder(hardware);
    if !caps.supports_encoder(encoder) {
        bail!("ffmpeg encoder {encoder} is unavailable on this worker");
    }

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
    fn nvenc_args_select_encoder_without_forcing_hardware_decode() {
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
        let args = build_args(Path::new("in.ts"), Path::new("out.ts"), &spec, &caps);
        let joined = args.join(" ");
        assert!(joined.contains("h264_nvenc"));
        assert!(!joined.contains("-hwaccel"));
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
        let args = build_args(Path::new("in.ts"), Path::new("out.ts"), &spec, &caps);
        assert!(args.iter().any(|a| a == "libsvtav1"));
    }

    #[test]
    fn hardware_selection_checks_the_selected_codec_encoder() {
        let caps = Capabilities {
            hwaccels: vec!["cuda".into()],
            encoders: vec!["h264_nvenc".into(), "libx265".into()],
        };
        assert_eq!(caps.best_hardware_for(Codec::H264), HardwarePref::Nvenc);
        assert_eq!(caps.best_hardware_for(Codec::Hevc), HardwarePref::Cpu);
    }

    #[tokio::test]
    async fn automatic_transcode_presets_produce_decodable_video_when_ffmpeg_supports_them() {
        let ffmpeg = Path::new("ffmpeg");
        let caps = match probe_capabilities(ffmpeg).await {
            Ok(caps) if caps.supports_encoder("libx264") && caps.supports_encoder("mpeg4") => caps,
            Ok(_) => {
                eprintln!("skipping real FFmpeg smoke test: libx264 or mpeg4 encoder unavailable");
                return;
            }
            Err(err) => {
                eprintln!("skipping real FFmpeg smoke test: {err:#}");
                return;
            }
        };

        let dir = tempfile::tempdir().expect("temporary directory");
        let input = dir.path().join("input.mp4");
        let fixture = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=128x72:r=10",
                "-t",
                "1",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&input)
            .status()
            .expect("run FFmpeg to make test input");
        assert!(fixture.success(), "FFmpeg failed to create test input");

        let presets = [
            ("video-720p", Codec::H264, 2_500_000, 1280, 720),
            ("video-1080p", Codec::H264, 5_000_000, 1920, 1080),
            ("video-4k", Codec::Hevc, 15_000_000, 3840, 2160),
        ];
        for (name, codec, bitrate_bps, width, height) in presets {
            if codec == Codec::Hevc && !caps.supports_encoder("libx265") {
                eprintln!("skipping {name}: libx265 encoder unavailable");
                continue;
            }
            let output = dir.path().join(format!("{name}.mp4"));
            let spec = TranscodeSpec {
                codec,
                bitrate_bps,
                width,
                height,
                keyframe_interval: 60,
                // Exercise the same automatic hardware selection used by CLI presets.
                hardware_pref: None,
                extra_ffmpeg_args: vec!["-threads".into(), "2".into()],
            };
            transcode(
                ffmpeg,
                &input,
                &output,
                &spec,
                &caps,
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|err| panic!("{name} transcode failed: {err:#}"));
            assert!(
                std::fs::metadata(&output).unwrap().len() > 0,
                "{name} output is empty"
            );

            let verify = std::process::Command::new(ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&output)
                .args(["-f", "null", "-"])
                .status()
                .expect("run FFmpeg to decode transcode output");
            assert!(verify.success(), "FFmpeg could not decode {name} output");
        }
    }
}
