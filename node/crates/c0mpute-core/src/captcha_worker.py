"""Captcha worker embedded into the c0mpute binary (ddddocr, ONNX, CPU).

Usage: python -c <this> solve   < {"kind": ..., "image": <base64>, ...}
       python -c <this> warmup  < {}

Kinds:
  image   text in a picture.        {image, charset?}            -> {text, confidence}
  slider  puzzle piece + backdrop.   {image (piece), background}  -> {x, y, box}
  gap     backdrop with the hole + the same backdrop without it.
                                     {image (with gap), background} -> {x, y}
  detect  click-the-characters.      {image}                      -> {boxes: [[x1,y1,x2,y2]]}

Prints one JSON object on stdout. ddddocr's models ship inside its wheel, so
nothing is downloaded at job time.
"""
import base64
import contextlib
import io
import json
import os
import sys


def b64(value):
    if not isinstance(value, str) or not value:
        raise SystemExit("missing image")
    return base64.b64decode(value, validate=True)


def quiet_ddddocr(**kwargs):
    # ddddocr prints a banner unless show_ad=False, and some versions print on
    # import; keep stdout clean for our one JSON line.
    with contextlib.redirect_stdout(io.StringIO()):
        import ddddocr

        return ddddocr.DdddOcr(show_ad=False, **kwargs)


def rounded(value):
    try:
        return round(float(value), 4)
    except (TypeError, ValueError):
        return None


def solve_image(request):
    ocr = quiet_ddddocr()
    charset = request.get("charset")
    if charset:
        ocr.set_ranges(charset)
    img = b64(request.get("image"))
    # ddddocr 1.6: {text, confidence (mean best-class softmax), ...}
    out = ocr.classification(img, probability=True)
    if isinstance(out, dict):
        return {"text": str(out.get("text") or ""), "confidence": rounded(out.get("confidence"))}
    return {"text": str(out), "confidence": None}


def center(out):
    target = out.get("target") or [None, None]
    return {
        "x": out.get("target_x", target[0]),
        "y": out.get("target_y", target[1] if len(target) > 1 else None),
        "confidence": rounded(out.get("confidence")),
    }


def solve_slider(request):
    # x, y: the centre of where the piece fits in the background.
    engine = quiet_ddddocr(det=False, ocr=False)
    return center(engine.slide_match(b64(request.get("image")), b64(request.get("background")), simple_target=True))


def solve_gap(request):
    engine = quiet_ddddocr(det=False, ocr=False)
    return center(engine.slide_comparison(b64(request.get("image")), b64(request.get("background"))))


def solve_detect(request):
    engine = quiet_ddddocr(det=True, ocr=False)
    boxes = engine.detection(b64(request.get("image")))
    return {"boxes": [[int(v) for v in box] for box in boxes]}


SOLVERS = {"image": solve_image, "slider": solve_slider, "gap": solve_gap, "detect": solve_detect}


def main():
    action = sys.argv[1]
    request = json.load(sys.stdin)
    os.environ.setdefault("OMP_NUM_THREADS", os.environ.get("C0MPUTE_JOB_CPU_THREADS") or "1")
    if action == "warmup":
        # Load both models once so a broken install never gets marked ready.
        quiet_ddddocr()
        quiet_ddddocr(det=True, ocr=False)
        import importlib.metadata

        result = {"engine": "ddddocr", "engine_version": importlib.metadata.version("ddddocr")}
    elif action == "solve":
        kind = request.get("kind")
        if kind not in SOLVERS:
            raise SystemExit(f"unknown captcha kind {kind!r}")
        result = SOLVERS[kind](request)
        result["kind"] = kind
    else:
        raise SystemExit(f"unknown action {action!r}")
    json.dump(result, sys.stdout, separators=(",", ":"), allow_nan=False)


main()
