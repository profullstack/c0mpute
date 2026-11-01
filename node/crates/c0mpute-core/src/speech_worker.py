"""Small CPU speech worker embedded into the c0mpute binary."""
import json
import math
import os
import sys
import wave
from pathlib import Path


def cpu_thread_count() -> int:
    configured = os.environ.get("C0MPUTE_SPEECH_CPU_THREADS")
    if configured:
        try:
            return max(1, int(configured))
        except ValueError:
            pass

    # Docker and most Linux services expose their CPU quota through cgroups.
    # Respect it because os.cpu_count() can report the whole host, which makes
    # ONNX Runtime create far too many threads inside a small container.
    for quota_path, period_path in [
        (Path("/sys/fs/cgroup/cpu.max"), None),
        (Path("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"), Path("/sys/fs/cgroup/cpu/cpu.cfs_period_us")),
        (Path("/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_quota_us"), Path("/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_period_us")),
    ]:
        try:
            if period_path is None:
                quota, period = quota_path.read_text().split()
                if quota == "max":
                    continue
                quota_us, period_us = int(quota), int(period)
            else:
                quota_us = int(quota_path.read_text())
                period_us = int(period_path.read_text())
            if quota_us > 0 and period_us > 0:
                return max(1, math.ceil(quota_us / period_us))
        except (OSError, ValueError):
            continue

    try:
        return max(1, len(os.sched_getaffinity(0)))
    except AttributeError:
        return max(1, os.cpu_count() or 1)


def session_options():
    import onnxruntime as ort

    options = ort.SessionOptions()
    options.intra_op_num_threads = cpu_thread_count()
    options.inter_op_num_threads = 1
    return options


def tts(root: Path, output: Path, request: dict) -> None:
    text = request["text"]
    engine = request["engine"]
    voice = request["voice"]
    if engine == "piper":
        import onnxruntime as ort
        from piper.config import PiperConfig
        from piper.voice import PiperVoice

        model = root / "models" / "piper" / f"{voice}.onnx"
        config_path = model.with_suffix(model.suffix + ".json")
        piper_config = PiperConfig.from_dict(json.loads(config_path.read_text(encoding="utf-8")))
        session = ort.InferenceSession(
            str(model), sess_options=session_options(), providers=["CPUExecutionProvider"]
        )
        piper_voice = PiperVoice(session=session, config=piper_config)
        with wave.open(str(output), "wb") as wav_file:
            piper_voice.synthesize_wav(text, wav_file)
        return
    if engine == "kokoro":
        import soundfile as sf
        import onnxruntime as ort
        import kokoro_onnx
        from kokoro_onnx import Kokoro

        # kokoro-onnx does not expose ONNX SessionOptions publicly. Replace
        # its small session factory so CPU jobs use the same quota-aware pool.
        kokoro_onnx.create_session = lambda path: ort.InferenceSession(
            path,
            sess_options=session_options(),
            providers=["CPUExecutionProvider"],
        )

        model_root = root / "models" / "kokoro"
        kokoro = Kokoro(str(model_root / "kokoro-v1.0.int8.onnx"), str(model_root / "voices-v1.0.bin"))
        try:
            audio, sample_rate = kokoro.create(
                text, voice=voice, speed=float(request.get("speed", 1.0)),
                lang=str(request.get("locale", "en-US")).lower(),
            )
            sf.write(str(output), audio, sample_rate, subtype="PCM_16")
        finally:
            kokoro.voices.close()
        return
    raise ValueError("unsupported TTS engine")


def stt(root: Path, audio: Path, output: Path, request: dict) -> None:
    from faster_whisper import WhisperModel

    model_name = request.get("model", "base")
    model = WhisperModel(
        model_name, device="cpu", compute_type="int8",
        cpu_threads=cpu_thread_count(), num_workers=1,
        download_root=str(root / "models" / "whisper"),
    )
    segments, info = model.transcribe(
        str(audio), beam_size=1,
        language=request.get("language") or None,
        vad_filter=True,
    )
    rows = [{"start": item.start, "end": item.end, "text": item.text.strip()} for item in segments]
    result = {
        "text": " ".join(item["text"] for item in rows).strip(),
        "language": info.language,
        "language_probability": info.language_probability,
        "segments": rows,
    }
    output.write_text(json.dumps(result, ensure_ascii=False), encoding="utf-8")


def main() -> None:
    action = sys.argv[1]
    root = Path(sys.argv[2])
    if action == "tts":
        tts(root, Path(sys.argv[3]), json.load(sys.stdin))
    elif action == "stt":
        stt(root, Path(sys.argv[3]), Path(sys.argv[4]), json.load(sys.stdin))
    else:
        raise ValueError("unknown speech action")


if __name__ == "__main__":
    main()
