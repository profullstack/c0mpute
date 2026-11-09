//! The broadcaster: `c0mpute stream go <input> --name <stream>`.
//!
//! Runs the local `ffmpeg` into HLS (one directory per rendition), watches
//! the playlists ffmpeg writes, and for every completed segment: stores its
//! chunks, slides the rendition's window, signs a new [`LiveRecord`] and
//! gossips it. Inputs are anything ffmpeg reads: a file (paced with `-re`),
//! an `rtmp://` / `srt://` / `udp://` URL, or `-` for stdin.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::playlist::{self, LocalPlaylist};
use super::record::{self, LiveBody, LiveRecord, Window};
use super::{LiveStream, Timing};

/// How the first rendition is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Copy streams as they are (no re-encode). Needs H.264/AAC-ish input
    /// that MPEG-TS carries; segments cut at the input's keyframes.
    Copy,
    /// Re-encode with libx264 + AAC, keyframes forced at segment boundaries.
    X264,
    /// `X264` for generated (lavfi) or raw input, `Copy` otherwise.
    Auto,
}

impl std::str::FromStr for Codec {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "copy" => Ok(Codec::Copy),
            "x264" | "h264" | "libx264" => Ok(Codec::X264),
            "auto" => Ok(Codec::Auto),
            other => bail!("unknown codec {other:?} (copy, x264 or auto)"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GoOptions {
    /// File path, ffmpeg URL (rtmp://, srt://, udp://, http://...) or `-`.
    pub input: String,
    pub name: String,
    /// ffmpeg input format (`-f`), e.g. `lavfi`, `mpegts`, `flv`.
    pub format: Option<String>,
    /// Target segment length, seconds (2-10).
    pub segment_secs: u32,
    /// Segments per rendition in the live window.
    pub window: usize,
    /// Add a 480p libx264 rendition next to the source.
    pub low: bool,
    pub codec: Codec,
    /// Pass `-listen 1` (receive an RTMP/SRT push instead of pulling).
    pub listen: bool,
    /// Also serve the stream locally (e.g. 127.0.0.1:7778) for a preview.
    pub bind: Option<std::net::SocketAddr>,
    /// After the input ends, stay up this long so relays fetch the tail.
    pub linger: Duration,
    /// Restart ffmpeg when a network input drops (with a discontinuity).
    pub reconnect: bool,
}

/// Is `input` a network URL (as opposed to a file or stdin)?
pub fn is_url(input: &str) -> bool {
    input.contains("://")
}

/// The ffmpeg command line for `opts`, writing rendition `i` to
/// `<work>/r<i>/index.m3u8`.
pub fn ffmpeg_args(opts: &GoOptions, work: &Path) -> Vec<String> {
    let seg = opts.segment_secs.clamp(2, 10);
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-y"].iter().map(|s| s.to_string()).collect();
    // Files and generators are read at native speed; live inputs already are.
    let paced = opts.input != "-" && !is_url(&opts.input);
    if paced {
        a.push("-re".into());
    }
    if opts.listen {
        a.extend(["-listen".into(), "1".into()]);
    }
    if let Some(f) = &opts.format {
        a.extend(["-f".into(), f.clone()]);
    }
    let input = if opts.input == "-" { "pipe:0".to_string() } else { opts.input.clone() };
    a.extend(["-i".into(), input]);

    let x264 = |a: &mut Vec<String>, extra: &[&str]| {
        a.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-pix_fmt",
                "yuv420p",
                "-sc_threshold",
                "0",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        a.extend(["-force_key_frames".into(), format!("expr:gte(t,n_forced*{seg})")]);
        a.extend(extra.iter().map(|s| s.to_string()));
        a.extend(["-c:a", "aac", "-b:a", "128k", "-ac", "2"].iter().map(|s| s.to_string()));
    };
    let hls = |a: &mut Vec<String>, dir: PathBuf| {
        a.extend(
            [
                "-f",
                "hls",
                "-hls_time",
                &seg.to_string(),
                "-hls_list_size",
                &(opts.window + 4).to_string(),
                "-hls_flags",
                "delete_segments+independent_segments+temp_file",
                "-hls_segment_type",
                "mpegts",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        a.extend(["-hls_segment_filename".into(), dir.join("seg%d.ts").to_string_lossy().into_owned()]);
        a.push(dir.join("index.m3u8").to_string_lossy().into_owned());
    };
    let codec = match opts.codec {
        Codec::Auto if opts.format.as_deref() == Some("lavfi") || opts.format.as_deref() == Some("rawvideo") => {
            Codec::X264
        }
        Codec::Auto => Codec::Copy,
        c => c,
    };
    a.extend(["-map", "0:v:0?", "-map", "0:a:0?"].iter().map(|s| s.to_string()));
    match codec {
        Codec::Copy => a.extend(["-c".into(), "copy".into()]),
        _ => x264(&mut a, &[]),
    }
    hls(&mut a, work.join("r0"));
    if opts.low {
        a.extend(["-map", "0:v:0?", "-map", "0:a:0?"].iter().map(|s| s.to_string()));
        x264(
            &mut a,
            &["-vf", "scale=-2:480", "-b:v", "1000k", "-maxrate", "1200k", "-bufsize", "2000k"],
        );
        hls(&mut a, work.join("r1"));
    }
    a
}

/// The broadcaster's signing state: one window per rendition and the next
/// `seq`. Pure bookkeeping; [`Broadcaster::push`] does the I/O.
pub struct Broadcaster {
    pub live: LiveStream,
    keypair: c0mpute_net::libp2p::identity::Keypair,
    pub name: String,
    pub windows: Vec<Window>,
    seq: u64,
    pub last: Option<LiveRecord>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Broadcaster {
    /// `renditions`: (name, resolution) per quality level.
    pub fn new(
        live: LiveStream,
        keypair: c0mpute_net::libp2p::identity::Keypair,
        name: &str,
        renditions: &[(&str, Option<String>)],
        window: usize,
    ) -> Result<Self> {
        if !record::valid_name(name) {
            bail!("stream name must be 1-63 of a-z, 0-9 and '-'");
        }
        Ok(Self {
            live,
            keypair,
            name: name.to_string(),
            windows: renditions
                .iter()
                .map(|(n, res)| Window::new(n, res.clone(), window))
                .collect(),
            seq: 0,
            last: None,
        })
    }

    pub fn stream_id(&self) -> String {
        record::stream_id_for(&record::owner_hex(&self.keypair).unwrap_or_default(), &self.name)
    }

    fn next_seq(&mut self) -> u64 {
        // Wall-clock based so a restarted broadcaster still wins, and
        // strictly increasing within a run.
        self.seq = now_ms().max(self.seq + 1);
        self.seq
    }

    fn sign(&mut self, ended: bool) -> Result<LiveRecord> {
        let seq = self.next_seq();
        let rec = LiveRecord::sign(
            &self.keypair,
            LiveBody {
                v: 1,
                stream_id: String::new(),
                name: self.name.clone(),
                owner: String::new(),
                seq,
                ts_ms: now_ms(),
                ended,
                renditions: self.windows.iter().map(|w| w.rendition.clone()).collect(),
            },
        )?;
        self.last = Some(rec.clone());
        Ok(rec)
    }

    /// Add a finished segment to rendition `idx`, then sign and gossip the
    /// new window.
    pub async fn push(&mut self, idx: usize, bytes: &[u8], dur_ms: u32, disc: bool) -> Result<LiveRecord> {
        let dur_ms = dur_ms.clamp(1, record::MAX_SEGMENT_MS);
        if bytes.len() > record::CHUNK_BYTES * record::MAX_CHUNKS_PER_SEGMENT {
            bail!(
                "segment of {} bytes is over the {} MiB limit; lower the bitrate or --segment-secs",
                bytes.len(),
                record::MAX_CHUNKS_PER_SEGMENT
            );
        }
        let (seg, pieces) = record::describe_segment(bytes, dur_ms, disc);
        self.live.store_segment(&pieces).await?;
        let w = self.windows.get_mut(idx).context("no such rendition")?;
        w.push(seg);
        let rec = self.sign(false)?;
        self.live.publish_record(&rec).await?;
        Ok(rec)
    }

    /// Re-sign and gossip the current window (a heartbeat for late joiners).
    pub async fn heartbeat(&mut self) -> Result<()> {
        if self.windows.iter().all(|w| w.rendition.segments.is_empty()) {
            return Ok(());
        }
        let rec = self.sign(false)?;
        self.live.publish_record(&rec).await
    }

    /// Mark the broadcast over (`EXT-X-ENDLIST`).
    pub async fn end(&mut self) -> Result<()> {
        if self.windows.iter().all(|w| w.rendition.segments.is_empty()) {
            return Ok(());
        }
        let rec = self.sign(true)?;
        self.live.publish_record(&rec).await
    }
}

/// Watches the playlists one ffmpeg run writes and reports new segments.
struct Watch {
    dirs: Vec<PathBuf>,
    /// Next ffmpeg media sequence expected per rendition.
    next: Vec<u64>,
    /// First segment of this run gets a discontinuity (not on the first run).
    disc_pending: Vec<bool>,
}

impl Watch {
    fn new(dirs: Vec<PathBuf>, restarted: bool) -> Self {
        let n = dirs.len();
        Self {
            dirs,
            next: vec![0; n],
            disc_pending: vec![restarted; n],
        }
    }

    /// New complete segments: (rendition, bytes, dur_ms, disc), and whether
    /// every playlist says the stream ended.
    fn poll(&mut self) -> (Vec<(usize, Vec<u8>, u32, bool)>, bool) {
        let mut out = Vec::new();
        let mut ended = !self.dirs.is_empty();
        for (i, dir) in self.dirs.iter().enumerate() {
            let pl: LocalPlaylist = std::fs::read_to_string(dir.join("index.m3u8"))
                .map(|t| playlist::parse_media_playlist(&t))
                .unwrap_or_default();
            ended &= pl.ended;
            for s in pl.segments {
                if s.seq < self.next[i] || s.uri.contains('/') || s.uri.contains("..") {
                    continue;
                }
                match std::fs::read(dir.join(&s.uri)) {
                    Ok(bytes) => {
                        let disc = std::mem::take(&mut self.disc_pending[i]) || s.disc;
                        out.push((i, bytes, s.dur_ms, disc));
                        self.next[i] = s.seq + 1;
                    }
                    // Deleted before we read it (we fell behind): skip on.
                    Err(_) => self.next[i] = s.seq + 1,
                }
            }
        }
        (out, ended)
    }
}

/// `c0mpute stream go`: broadcast until the input ends (or ctrl-c).
pub async fn go(opts: GoOptions) -> Result<()> {
    let ffmpeg = crate::supervisor::which_ffmpeg()
        .context("ffmpeg is not on PATH; the broadcaster needs it (relays and viewers do not)")?;
    if !record::valid_name(&opts.name) {
        bail!("--name must be 1-63 of a-z, 0-9 and '-'");
    }
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let data = crate::config::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let keypair = c0mpute_net::identity::load_or_create(&data.join("buyer"))?;
    let live = LiveStream::start(supervisor.libp2p.clone(), supervisor.store.clone(), Timing::default()).await?;
    if let Some(bind) = opts.bind {
        let app = live.router();
        let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("bind {bind}"))?;
        tokio::spawn(async move { axum::serve(listener, app).await });
        eprintln!("local preview: http://{bind}/live/<stream_id>/index.m3u8");
    }
    let mut renditions: Vec<(&str, Option<String>)> = vec![("src", None)];
    if opts.low {
        renditions.push(("480p", None));
    }
    let mut b = Broadcaster::new(live.clone(), keypair, &opts.name, &renditions, opts.window)?;
    let id = b.stream_id();
    let work = data.join("live").join(format!("{}-{}", opts.name, std::process::id()));
    println!("stream:   {id}");
    println!("playlist: /live/{id}/index.m3u8   (on any c0mpute gateway, e.g. http://<gateway>:7777/live/{id}/index.m3u8)");
    if let Some(bind) = opts.bind {
        println!("preview:  http://{bind}/live/{id}/index.m3u8");
    }
    // Let the swarm reach peers before the first gossip.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut restarted = false;
    let result = loop {
        let _ = std::fs::remove_dir_all(&work);
        let dirs: Vec<PathBuf> = (0..renditions.len()).map(|i| work.join(format!("r{i}"))).collect();
        for d in &dirs {
            std::fs::create_dir_all(d)?;
        }
        let args = ffmpeg_args(&opts, &work);
        info!(?args, "live: starting ffmpeg");
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.args(&args).kill_on_drop(true);
        if opts.input != "-" {
            cmd.stdin(std::process::Stdio::null());
        }
        let mut child = cmd.spawn().context("start ffmpeg")?;
        let mut watch = Watch::new(dirs, restarted);
        let mut last_push = Instant::now();
        let exit = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break None,
                status = child.wait() => break Some(status),
                _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            }
            let (segs, _) = watch.poll();
            for (i, bytes, dur, disc) in segs {
                match b.push(i, &bytes, dur, disc).await {
                    Ok(rec) => {
                        let r = &rec.body.renditions[i];
                        eprintln!(
                            "  {} #{} {:.1}s {} KiB",
                            r.name,
                            r.media_seq + r.segments.len() as u64 - 1,
                            dur as f64 / 1000.0,
                            bytes.len() / 1024
                        );
                        last_push = Instant::now();
                    }
                    Err(e) => warn!(err = %e, "live: segment not published"),
                }
            }
            if last_push.elapsed() >= Duration::from_secs(opts.segment_secs.max(2) as u64) {
                let _ = b.heartbeat().await;
                last_push = Instant::now();
            }
        };
        // Pick up segments ffmpeg finished on its way out.
        let (segs, _) = watch.poll();
        for (i, bytes, dur, disc) in segs {
            let _ = b.push(i, &bytes, dur, disc).await;
        }
        match exit {
            None => break Ok(()),
            Some(status) => {
                let status = status.context("wait for ffmpeg")?;
                if opts.reconnect && is_url(&opts.input) {
                    eprintln!("ffmpeg exited ({status}); reconnecting in 2 s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    restarted = true;
                    continue;
                }
                if !status.success() && b.last.is_none() {
                    break Err(anyhow::anyhow!("ffmpeg failed ({status}) before producing a segment"));
                }
                break Ok(());
            }
        }
    };
    let _ = std::fs::remove_dir_all(&work);
    b.end().await?;
    if result.is_ok() && b.last.is_some() && !opts.linger.is_zero() {
        eprintln!("broadcast ended; staying up {} s so relays can fetch the tail", opts.linger.as_secs());
        let deadline = Instant::now() + opts.linger;
        while Instant::now() < deadline {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            let _ = b.end().await;
        }
    }
    live.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(input: &str) -> GoOptions {
        GoOptions {
            input: input.into(),
            name: "test".into(),
            format: None,
            segment_secs: 4,
            window: 6,
            low: false,
            codec: Codec::Auto,
            listen: false,
            bind: None,
            linger: Duration::ZERO,
            reconnect: true,
        }
    }

    #[test]
    fn ffmpeg_args_for_inputs() {
        let w = Path::new("/w");
        let file = ffmpeg_args(&opts("in.mp4"), w);
        assert!(file.contains(&"-re".to_string()), "files are paced");
        assert!(file.windows(2).any(|p| p == ["-c", "copy"]), "copy by default");
        assert!(file.windows(2).any(|p| p == ["-hls_time", "4"]));
        assert!(file.windows(2).any(|p| p == ["-hls_list_size", "10"]));
        assert_eq!(file.last().unwrap(), "/w/r0/index.m3u8");

        let rtmp = ffmpeg_args(&opts("rtmp://example.com/live/key"), w);
        assert!(!rtmp.contains(&"-re".to_string()), "live inputs are not paced");
        let stdin = ffmpeg_args(&opts("-"), w);
        assert!(stdin.windows(2).any(|p| p == ["-i", "pipe:0"]));
        assert!(!stdin.contains(&"-re".to_string()));

        let mut o = opts("testsrc=size=640x360:rate=25");
        o.format = Some("lavfi".into());
        o.low = true;
        let generated = ffmpeg_args(&o, w);
        assert!(generated.windows(2).any(|p| p == ["-f", "lavfi"]));
        assert!(generated.windows(2).any(|p| p == ["-c:v", "libx264"]), "generated input is encoded");
        assert!(generated.contains(&"/w/r1/index.m3u8".to_string()), "second rendition");
        assert!(generated.windows(2).any(|p| p == ["-vf", "scale=-2:480"]));

        let mut o = opts("rtmp://0.0.0.0:1935/live/k");
        o.listen = true;
        assert!(ffmpeg_args(&o, w).windows(2).any(|p| p == ["-listen", "1"]));
    }

    #[test]
    fn watch_reads_new_segments_once_and_marks_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("r0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seg0.ts"), b"zero").unwrap();
        std::fs::write(dir.join("seg1.ts"), b"one").unwrap();
        std::fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\nseg0.ts\n#EXTINF:4.0,\nseg1.ts\n",
        )
        .unwrap();
        let mut w = Watch::new(vec![dir.clone()], true);
        let (segs, ended) = w.poll();
        assert!(!ended);
        assert_eq!(segs.len(), 2);
        assert!(segs[0].3, "first segment after a restart is a discontinuity");
        assert!(!segs[1].3);
        assert_eq!(segs[1].1, b"one");
        assert!(w.poll().0.is_empty(), "nothing new");
        std::fs::write(dir.join("seg2.ts"), b"two").unwrap();
        std::fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\nseg1.ts\n#EXTINF:3.5,\nseg2.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        let (segs, ended) = w.poll();
        assert!(ended);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].2, 3500);
    }
}
