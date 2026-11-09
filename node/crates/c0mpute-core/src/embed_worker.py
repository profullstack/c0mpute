"""Text-embedding worker embedded into the c0mpute binary (fastembed, ONNX, CPU).

Usage: python -c <this> <cache_dir> embed    < {"model": ..., "texts": [...]}
       python -c <this> <cache_dir> warmup   < {"model": ...}

Prints one JSON object on stdout: {"model", "dims", "vectors"}.
"""
import json
import os
import sys


def main() -> None:
    cache_dir, action = sys.argv[1], sys.argv[2]
    request = json.load(sys.stdin)
    from fastembed import TextEmbedding

    threads = int(os.environ.get("C0MPUTE_JOB_CPU_THREADS") or 0) or None
    model = TextEmbedding(model_name=request["model"], cache_dir=cache_dir, threads=threads)
    texts = request.get("texts") or []
    if action == "warmup":
        texts = ["c0mpute embeddings warmup"]
    elif action != "embed":
        raise SystemExit(f"unknown action {action!r}")
    vectors = [[round(float(x), 7) for x in vector] for vector in model.embed(texts, batch_size=32)]
    dims = len(vectors[0]) if vectors else 0
    json.dump(
        {"model": request["model"], "dims": dims, "vectors": vectors},
        sys.stdout,
        separators=(",", ":"),
        allow_nan=False,
    )


main()
