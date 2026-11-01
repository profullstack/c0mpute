"""Small CPU speech worker embedded into the c0mpute binary."""
import json
import subprocess
import sys
from pathlib import Path


def tts(root: Path, output: Path, request: dict) -> None:
    text = request["text"]
    engine = request["engine"]
    voice = request["voice"]
    if engine == "piper":
        model = root / "models" / "piper" / f"{voice}.onnx"
        subprocess.run(
            [sys.executable, "-m", "piper", "--model", str(model), "--output_file", str(output)],
            input=text + "\n", text=True, check=True,
        )
        return
    if engine == "kokoro":
        import soundfile as sf
        from kokoro_onnx import Kokoro

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
        download_root=str(root / "models" / "whisper"),
    )
    segments, info = model.transcribe(
        str(audio), beam_size=5,
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
