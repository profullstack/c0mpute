# c0mpute video worker on a rented GPU container (Salad)

`Dockerfile` here = the video runner image + the released `c0mpute` binary.
The entrypoint warms the Hugging Face weights for `C0MPUTE_PREFETCH_MODELS`
(default `wan2.1-t2v-1.3b`, 29 GB), then runs
`c0mpute worker start --roles video --no-storage --no-hosting` with
`C0MPUTE_VIDEO_RUNNER=local`, so jobs exec `runner.py` in the container
instead of `docker run` (a rented container has no docker inside).
Outbound-only networking is enough: the node joins through the relays.

Build: `.github/workflows/video-salad.yml` (manual dispatch, input = release
tag). It pushes to `ttl.sh/<random>:24h` (anonymous, public, expires in a day,
free) because ghcr packages are private by default and a rented host would
need a registry token; the image holds no secrets.

Container group: 1 replica, the GPU class you want, 8 vCPU / 32 GB RAM (the
text encoder is loaded on the CPU first), enough disk for the weights, port
8080 behind the container gateway (auth on) for `/boot.log`, `/worker.log`
and `/bench.log`. Delete the group when done: a rented card bills by the
minute, image pull included.
