---
dip: 0009
title: "Mojo for GPU/kernel-shaped compute (when applicable)"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: deferred until first kernel-shaped workload lands
supersedes:
superseded-by:
---

## Summary

When c0mpute needs custom GPU kernels — e.g. AI upscaling, frame
interpolation, novel inference runtimes, encoder rate-control tuning —
the implementation language is **Mojo**, not CUDA C, not a hand-rolled
Rust GPU shader, not a fragile Python+Numba stack.

This is a forward-looking decision. We have **no Mojo code in the
repo today**; v1 ships entirely in Rust + Bun + FFmpeg. The DIP exists
to lock the language choice before a contributor reaches for the
"obvious" alternatives.

## Motivation

Two observations:

1. **The c0mpute network will accumulate kernel-shaped workloads.**
   Once transcode and inference are stable, the natural next workloads
   are differentiated GPU work: ML upscaling, super-resolution,
   denoising, custom inference quantization, video diffusion, etc.
   Each one wants tight GPU code.

2. **Mojo is the cleanest path** for that work today. Python-superset
   syntax (so kernels can borrow from the existing Python ML
   ecosystem), AOT compilation, first-class GPU + SIMD targeting. The
   alternatives — CUDA-only C++, Triton, or shoehorning everything
   through ONNX runtimes — each have worse trade-offs around
   developer experience and portability.

The only real gotcha: Mojo's compiler is currently proprietary
(Modular). Their public roadmap commits to open-sourcing the compiler
once stable. We adopt Mojo on that basis, accepting the temporary
build-time dependency on Modular's distribution.

## Detailed design

### Where Mojo fits

| Workload                                  | Today (v1)                | Future (Mojo) |
|-------------------------------------------|---------------------------|---------------|
| Transcode (FFmpeg)                        | FFmpeg subprocess         | unchanged     |
| Quality verification (VMAF)               | FFmpeg `libvmaf` filter   | unchanged     |
| AI inference (LLM)                        | infernet runtime          | infernet runtime, possibly Mojo-backed |
| AI upscaling (Real-ESRGAN-style)          | not in v1                 | Mojo kernel   |
| Frame interpolation                       | not in v1                 | Mojo kernel   |
| Custom rate-control / encoder tuning      | not in v1                 | Mojo kernel   |
| Novel zero-knowledge / proof-of-compute   | not in v1                 | Mojo kernel possible |

The principle: if FFmpeg can do it, FFmpeg does it (PRD §9 still
applies). When we need genuinely custom GPU compute, we reach for
Mojo, not CUDA.

### Where Mojo does NOT fit

- The `c0mpute` / `coinpay` / `infernet` CLI binaries — Rust, full stop.
  Mojo isn't a systems-programming replacement.
- The coordinator API — Bun + Hono. No reason to involve Mojo.
- Network protocol layer — Rust + libp2p.
- The TUI / web — TS / React.

Mojo is a *workload* language, not an *infrastructure* language.

### Repo layout (when Mojo lands)

A Mojo-using plugin would look like:

```
plugins/upscale/
├── module.toml          # mode = "container" with a runner image
├── runner/              # Mojo source
│   ├── upscale.mojo
│   └── ...
├── runner-image/        # OCI image build (Dockerfile + entrypoint)
└── README.md
```

The runner is built into a signed OCI image. c0mpute dispatches the
job into the container; the container runs the Mojo binary. We don't
embed Mojo into the c0mpute host binary itself — same isolation model
as we use for FFmpeg today.

### License compatibility

Mojo's compiler is closed-source today. Our code under
MIT (per the licensing decision in the project
README) is unaffected — Mojo source we write stays under our license;
only the toolchain is currently proprietary. If Modular doesn't open-
source the compiler on a reasonable timeline, we re-evaluate this DIP.

## Alternatives considered

**CUDA C++.** What everyone reaches for. The DX is rough (build
systems, ABI, no shared module ecosystem with Python), and it's
NVIDIA-locked at the source level. Mojo abstracts the device target.

**Triton (OpenAI).** Excellent for LLM-shaped kernels. Less general,
worse tooling for non-attention workloads, narrower community than
Mojo. Could still co-exist for specific inference paths.

**Plain Python + Numba / Cython.** Python is fine for ML control
flow but performance ceiling is too low for the kernels we'd actually
write. Numba helps but ages poorly.

**Pure Rust (`cudarc`, `wgpu`, etc.).** Would let us keep one
language. The kernel-authoring DX in Rust GPU is genuinely worse than
Mojo today; a Mojo + Rust split is fine.

**Wait until Mojo is FOSS before committing.** Conservative. The
risk: a contributor reaches for CUDA tomorrow because no one wrote
this DIP, and we accumulate language drift. Better to commit now and
revisit if Modular drags on open-sourcing.

## Migration & rollout

Nothing to migrate today. When the first kernel-shaped workload
arrives:

1. Add a `plugins/<id>/runner/` Mojo source tree.
2. Vendor / install the Mojo SDK in CI.
3. Build the runner into a signed OCI image; publish to the c0mpute
   release feed.
4. Distribute as a `mode = "container"` module per DIP-0006.
5. Update DIP status from "deferred until first kernel-shaped
   workload lands" to a real implementation pointer.

## Open questions

- **Modular's open-source timeline.** If it slips materially we revisit.
- **CI runners with GPUs.** Will need GPU-accelerated CI when the
  first Mojo workload lands; out of scope here.
- **Rust ↔ Mojo FFI.** If a future host-side component needs to call
  Mojo directly (vs. via the container boundary), we'd need to design
  the FFI. For v1+1 we punt by always going through the container.

## Out of scope

- Adopting Mojo for *any* code we have today. Rust + Bun + FFmpeg are
  fine for v1.
- The Modular Engine inference runtime specifically. That's a separate
  evaluation against Ollama / vLLM / llama.cpp inside the infernet
  plugin and doesn't belong in this DIP.
