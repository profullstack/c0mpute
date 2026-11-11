"""Small CPU speech worker embedded into the c0mpute binary."""
import json
import math
import os
import sys
import time
import wave
from pathlib import Path


def available_cpu_capacity() -> float:
    quota_capacity = None
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
                quota_capacity = quota_us / period_us
                break
        except (OSError, ValueError):
            continue

    try:
        affinity_capacity = float(len(os.sched_getaffinity(0)))
    except (AttributeError, OSError):
        affinity_capacity = float(os.cpu_count() or 1)
    return min(quota_capacity, affinity_capacity) if quota_capacity else affinity_capacity


def cpu_thread_count() -> int:
    configured = os.environ.get("C0MPUTE_SPEECH_CPU_THREADS")
    if not configured:
        configured = os.environ.get("C0MPUTE_JOB_CPU_THREADS")
    if configured:
        try:
            return max(1, int(configured))
        except ValueError:
            pass
    return max(1, math.ceil(available_cpu_capacity() / 2))


def session_options():
    import onnxruntime as ort

    options = ort.SessionOptions()
    options.intra_op_num_threads = cpu_thread_count()
    options.inter_op_num_threads = 1
    return options


def kokoro_model(model_root: Path) -> Path:
    """The full-precision Kokoro when it is installed, else the int8 build.

    int8 sounds fuzzy and loses the top octave, and on ONNX Runtime CPU it is
    not even faster; it stays only as the fallback for nodes mid-upgrade.
    """
    full = model_root / "kokoro-v1.0.onnx"
    return full if full.is_file() else model_root / "kokoro-v1.0.int8.onnx"


def default_whisper_model(root: Path) -> str:
    """Whisper small when provisioned (far fewer word errors than base), else base."""
    cache = root / "models" / "whisper"
    return "small" if any(cache.glob("models--Systran--faster-whisper-small*")) else "base"


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
        kokoro = Kokoro(str(kokoro_model(model_root)), str(model_root / "voices-v1.0.bin"))
        try:
            audio, sample_rate = kokoro.create(
                text, voice=voice, speed=float(request.get("speed", 1.0)),
                lang=str(request.get("locale", "en-US")).lower(),
            )
            sf.write(str(output), audio, sample_rate, subtype="PCM_16")
        finally:
            kokoro.voices.close()
        return
    if engine == "pocket":
        if str(request.get("locale", "en-US")).lower().split("-", 1)[0] != "en":
            raise ValueError("Pocket TTS currently supports English voice jobs only")

        import os
        import numpy as np
        import torch
        from pocket_tts import TTSModel
        from scipy.io import wavfile

        torch.set_num_threads(max(1, int(os.environ.get("OMP_NUM_THREADS", "2"))))
        model = TTSModel.load_model(language="english_2m_2026-10")
        voice_state = model.get_state_for_audio_prompt(voice)
        audio = model.generate_audio(voice_state, text).detach().cpu().numpy().reshape(-1)
        pcm = (np.clip(audio, -1.0, 1.0) * 32767.0).astype(np.int16)
        wavfile.write(str(output), model.sample_rate, pcm)
        return
    if engine == "chatterbox":
        import soundfile as sf

        audio, sample_rate = ChatterboxSpeaker(root).create(
            text, voice=voice, lang=str(request.get("locale", "en-US")).lower(),
            exaggeration=float(request.get("exaggeration", 0.6)),
        )
        sf.write(str(output), audio, sample_rate, subtype="PCM_16")
        return
    raise ValueError("unsupported TTS engine")


def stt(root: Path, audio: Path, output: Path, request: dict) -> None:
    from faster_whisper import WhisperModel

    model_name = request.get("model") or default_whisper_model(root)
    model = WhisperModel(
        model_name, device="cpu", compute_type="int8",
        cpu_threads=cpu_thread_count(), num_workers=1,
        download_root=str(root / "models" / "whisper"),
    )
    segments, info = model.transcribe(
        str(audio), beam_size=int(request.get("beam_size", 5)),
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


def load_kokoro(root: Path):
    """Load Kokoro once and hand back a `create`-able model.

    Spawning this process and loading the model is most of what a one-shot
    `tts` call spends its time on, which is why `serve` does it once and then
    answers utterance after utterance from the loaded model.
    """
    import onnxruntime as ort
    import kokoro_onnx
    from kokoro_onnx import Kokoro

    kokoro_onnx.create_session = lambda path: ort.InferenceSession(
        path, sess_options=session_options(), providers=["CPUExecutionProvider"]
    )
    model_root = root / "models" / "kokoro"
    return Kokoro(
        str(kokoro_model(model_root)), str(model_root / "voices-v1.0.bin")
    )


class PocketSpeaker:
    """Pocket TTS held in memory, with each voice's prompt state cached.

    Pocket is the default agent voice (2026-11-11): far more natural prosody
    than Kokoro (Anthony: "its still computerish. we need to be more human"),
    and warm it runs about 1x realtime on 8 CPU threads.
    """

    def __init__(self) -> None:
        import torch
        from pocket_tts import TTSModel

        torch.set_num_threads(max(1, int(os.environ.get("OMP_NUM_THREADS", str(cpu_thread_count())))))
        self.model = TTSModel.load_model(language="english_2m_2026-10")
        self.states: dict = {}

    def create(self, text: str, voice: str, speed: float = 1.0, lang: str = "en-us"):
        import numpy as np

        if lang.split("-", 1)[0] != "en":
            raise ValueError("Pocket TTS currently supports English voice jobs only")
        state = self.states.get(voice)
        if state is None:
            state = self.states[voice] = self.model.get_state_for_audio_prompt(voice)
        audio = self.model.generate_audio(state, text).detach().cpu().numpy().reshape(-1)
        return np.clip(audio, -1.0, 1.0).astype(np.float32), self.model.sample_rate


class ChatterboxSpeaker:
    """Chatterbox (Resemble AI, MIT) on the GPU: the expressive voices.

    Each voice is cloned from a clean reference clip; `exaggeration` turns the
    emotional range up or down (0.5 neutral, 0.6 our default, ~1.0 animated).
    """

    def __init__(self, root: Path) -> None:
        from chatterbox.tts import ChatterboxTTS

        self.model = ChatterboxTTS.from_pretrained(device="cuda")
        self.voices = root / "models" / "chatterbox" / "voices"

    def create(self, text: str, voice: str, speed: float = 1.0, lang: str = "en-us",
               exaggeration: float = 0.6):
        import numpy as np

        if lang.split("-", 1)[0] != "en":
            raise ValueError("Chatterbox serves English voice jobs only")
        reference = self.voices / f"{voice}.wav"
        if not reference.is_file():
            raise ValueError(f"no Chatterbox reference clip for {voice}")
        exaggeration = min(1.5, max(0.25, float(exaggeration)))
        # Livelier delivery wants a lower cfg weight, or the pacing rushes.
        cfg_weight = 0.5 if exaggeration <= 0.5 else 0.35
        # Sampling occasionally yields NaN audio (seen 2 of 4 tries on CPU,
        # 2026-11-11; the watermark step then raises "not finite"). A fresh
        # sample almost always comes out clean, so retry before failing.
        last = None
        for _attempt in range(3):
            try:
                wav = self.model.generate(text, audio_prompt_path=str(reference),
                                          exaggeration=exaggeration, cfg_weight=cfg_weight)
                audio = wav.squeeze().detach().cpu().numpy().astype(np.float32)
                if np.isfinite(audio).all() and np.abs(audio).max() > 1e-4:
                    break
                last = RuntimeError("Chatterbox produced non-finite or silent audio")
            except Exception as error:  # noqa: BLE001 - retry, then report
                last = error
        else:
            raise RuntimeError(f"Chatterbox failed 3 times: {last}")
        peak = float(np.abs(audio).max() or 1.0)
        if peak > 0.97:  # never hand back a clipped line
            audio = audio * (0.97 / peak)
        return audio, self.model.sr


def serve(root: Path, engine: str = "kokoro") -> None:
    """Answer utterances from a model held in memory, one JSON line each.

    Protocol, so a caller never has to guess how far along we are:

        <- {"ready": true, "engine": "kokoro", "load_ms": 1234}
        -> {"id": "1", "text": "hello", "voice": "am_michael", "out": "/path.wav"}
        <- {"id": "1", "ok": true, "seconds": 0.8, "ms": 210}
        -> {"op": "close"}

    A failed utterance answers {"ok": false, "error": ...} and the loop stays
    up: one bad line should not cost the caller its warm model.
    """
    import soundfile as sf

    started = time.monotonic()
    try:
        if engine == "pocket":
            kokoro = PocketSpeaker()
        elif engine == "chatterbox":
            kokoro = ChatterboxSpeaker(root)
        else:
            kokoro = load_kokoro(root)
    except Exception as error:  # noqa: BLE001 - tell the caller why, then stop
        print(json.dumps({"ready": False, "engine": engine, "error": f"{error}"[:300]}), flush=True)
        return
    load_ms = int((time.monotonic() - started) * 1000)
    print(json.dumps({"ready": True, "engine": engine, "load_ms": load_ms}), flush=True)

    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except ValueError:
            print(json.dumps({"ok": False, "error": "not json"}), flush=True)
            continue
        if request.get("op") == "close":
            break
        request_id = str(request.get("id", ""))
        began = time.monotonic()
        try:
            output = Path(request["out"])
            extra = {"exaggeration": float(request.get("exaggeration", 0.6))} if engine == "chatterbox" else {}
            audio, sample_rate = kokoro.create(
                request["text"],
                voice=request.get("voice", "bill_boerst" if engine == "pocket" else "am_michael"),
                speed=float(request.get("speed", 1.0)),
                lang=str(request.get("locale", "en-US")).lower(),
                **extra,
            )
            sf.write(str(output), audio, sample_rate, subtype="PCM_16")
            print(
                json.dumps(
                    {
                        "id": request_id,
                        "ok": True,
                        "seconds": round(len(audio) / float(sample_rate), 3),
                        "ms": int((time.monotonic() - began) * 1000),
                    }
                ),
                flush=True,
            )
        except Exception as error:  # noqa: BLE001 - report, keep serving
            print(
                json.dumps({"id": request_id, "ok": False, "error": f"{error}"[:300]}),
                flush=True,
            )

    try:
        if engine == "kokoro":
            kokoro.voices.close()
    except Exception:  # noqa: BLE001 - we are on the way out anyway
        pass


def main() -> None:
    action = sys.argv[1]
    root = Path(sys.argv[2])
    if action == "tts":
        tts(root, Path(sys.argv[3]), json.load(sys.stdin))
    elif action == "stt":
        stt(root, Path(sys.argv[3]), Path(sys.argv[4]), json.load(sys.stdin))
    elif action == "serve":
        serve(root, sys.argv[3] if len(sys.argv) > 3 else "kokoro")
    else:
        raise ValueError("unknown speech action")


if __name__ == "__main__":
    main()
