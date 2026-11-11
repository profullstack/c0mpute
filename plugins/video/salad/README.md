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

Buyers: c0mpute has no relay, and a rented container cannot be dialed, so
the buyer gateway that sends the jobs must be reachable: run
`c0mpute speech serve` with `C0MPUTE_P2P_LISTEN=/ip4/0.0.0.0/tcp/<port>`
and `C0MPUTE_P2P_EXTERNAL=/ip4/<public ip>/tcp/<port>` and that port open.
A gateway on a random firewalled port wins the auction and then fails with
"fetch the sealed spec from the buyer: Failed to dial the requested peer".

Note: the gateway clips `wan2.1-t2v-1.3b` requests to the CPU size
(33 frames of 480x272) whatever the GPU could do; ask for
`wan2.2-ti2v-5b` for a full-size clip.

First run (2026-11-11, RTX 3090, Salad high): image pull 8.5 min, 29 GB
prefetch 9.5 min, 1.3B clip (33f 480x272) rendered in 53 s.
