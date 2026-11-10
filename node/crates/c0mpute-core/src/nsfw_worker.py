"""c0mpute NSFW output classifier (SFW jobs only).

argv: <model.onnx> <image>... | <model.onnx> --selftest
Prints {"scores": [p_nsfw, ...]} on stdout, one probability per image.

Model: AdamCodd/vit-base-nsfw-detector (Apache-2.0), ViT-base at 384x384,
labels {0: sfw, 1: nsfw}; preprocessing from its preprocessor_config.json:
bilinear resize to 384x384, rescale 1/255, normalize mean 0.5 std 0.5.
"""
import json
import os
import sys

import numpy as np
import onnxruntime as ort
from PIL import Image

SIZE = 384


def tensor(img):
    img = img.convert("RGB").resize((SIZE, SIZE), Image.BILINEAR)
    x = (np.asarray(img, dtype=np.float32) / 255.0 - 0.5) / 0.5
    return x.transpose(2, 0, 1)[None, ...]


def main():
    model, paths = sys.argv[1], sys.argv[2:]
    opts = ort.SessionOptions()
    threads = int(os.environ.get("C0MPUTE_JOB_CPU_THREADS", "2") or "2")
    opts.intra_op_num_threads = max(1, min(threads, 8))
    sess = ort.InferenceSession(model, sess_options=opts, providers=["CPUExecutionProvider"])
    name = sess.get_inputs()[0].name
    if paths == ["--selftest"]:
        images = [Image.new("RGB", (64, 64), (90, 140, 200))]
    else:
        images = [Image.open(p) for p in paths]
    scores = []
    for img in images:
        logits = sess.run(None, {name: tensor(img)})[0][0].astype(np.float64)
        e = np.exp(logits - logits.max())
        scores.append(float((e / e.sum())[1]))
    print(json.dumps({"scores": scores}))


if __name__ == "__main__":
    main()
