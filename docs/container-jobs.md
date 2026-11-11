# Container jobs: remote builds and runs

`c0mpute build` and `c0mpute run` send a docker build, or one command in a
container, to a builder on the network and bring the result back: the image,
or the files you name. There is no git step and no registry: the input is a
tarball of any directory. The protocol is [DIP 0033](../dips/0033-container-run.md)
(workload `container.run`).

Use it to keep heavy builds (Rust `target/`, Next.js caches, BuildKit cache)
off a small or busy machine, or to run tests on a bigger one.

## Quick start

On the builder (a machine with docker, buildx and a big disk):

```sh
c0mpute containers allow <buyer peer id>      # who may send it jobs
# config.toml: roles = [..., "containers"] and, ideally,
#   [containers]
#   work_dir = "/mnt/big/c0mpute-containers"
c0mpute worker restart
c0mpute status                                 # containers: serving ...
```

Give the builder a dialable p2p port (`C0MPUTE_P2P_LISTEN=/ip4/0.0.0.0/tcp/46347`
in the worker's environment, and open it in the firewall) and name it by
multiaddr on the buyer side, `/ip4/<ip>/tcp/46347/p2p/<peer id>`: a new
workload's topic is not relayed by every seed yet, and the buyer connects to
the named builders directly before it publishes the offer.

On the machine that wants the build:

```sh
c0mpute containers id                          # the peer id to give the builder
c0mpute containers add-builder /ip4/<ip>/tcp/46347/p2p/<builder peer id>
c0mpute build . --load -t myapp:latest         # image built remotely, loaded here
c0mpute build . -f docker/Dockerfile --build-arg NEXT_PUBLIC_URL=https://x.com -o image.tar.gz
c0mpute run --image node:22@sha256:<digest> --in . --out dist -- sh -c 'npm ci && npm run build'
c0mpute run --build . --out coverage -- make test   # build, then run in the result
```

`build` prints the image id on stdout; `run` exits with the remote
command's exit code and unpacks each `--out` path into `--dest` (default
`.`). Both stream the builder's log while the job runs. `--json` prints the
whole result.

A directory passed to `build` is packed with its `.dockerignore` applied
where GNU tar can do it exactly (docker applies it again on the builder). You
can also pass your own tarball, e.g. `git archive --format=tar.gz HEAD >
ctx.tgz && c0mpute build ctx.tgz`.

## Who builds for whom

Closed by default, both ways:

- A builder serves the identities on its own machine and the peer ids in
  `[containers] buyers` (`c0mpute containers allow|deny`). `c0mpute
  containers open` serves anyone; do not, unless the box is disposable: a
  build runs the buyer's Dockerfile with network access.
- A buyer sends jobs only to `--peer <id>`, `[containers] builders` or
  `C0MPUTE_BUILDERS=a,b`. `--any-worker` accepts anyone, and anyone could
  hand you a backdoored image. For deploys, use your own nodes.

The buyer identity lives in `<data dir>/build` (override with
`C0MPUTE_BUILD_IDENTITY`). Builds from one machine take turns on it.

## Limits and safety on the builder

`[containers]` in config.toml:

| key | default | meaning |
|---|---|---|
| `work_dir` | `<data dir>/containers` | per-job scratch (context, saved image, outputs) |
| `max_jobs` | 1 | jobs at once |
| `max_cpus`, `max_memory_mb` | half the machine | most a run step may ask for |
| `max_disk_gb` | 100 | most scratch one job may ask for |
| `max_timeout_secs` | 10800 | longest job |
| `min_free_gb` | min(20 GiB, 10%) | free space kept on the scratch and docker disks |
| `cache_gb` | 20 | BuildKit cache kept between builds |
| `buyers` | [] | trusted buyer peer ids |
| `open` | false | serve anyone |
| `egress_guard` | true | iptables guard on the run-step bridge (root only) |
| `guard_build_bridge` | none | also guard e.g. `docker0` (BuildKit RUN steps) |
| `builders` | [] | buyer side: where this machine sends jobs |

- Builds use the docker daemon's own BuildKit: no privileged builder
  container, no `--allow` entitlements, no secrets or ssh forwarding.
- Run steps: `--cpus`, `--memory` (no swap), `--pids-limit 4096`, all
  capabilities dropped except the eight package managers need,
  `no-new-privileges`, no volumes; files go in and out with `docker cp`.
- With root and `egress_guard`, run steps cannot reach private, loopback,
  link-local (cloud metadata), CGNAT or multicast addresses, or the host.
  Builds are guarded only with `guard_build_bridge`, which affects every
  default-network container on that host. Every result says which guard
  was up.
- A watchdog stops a job whose scratch outgrows `disk_mb` or that pushes
  either disk under `min_free_gb`. Scratch, the job's container and image
  are removed after every job; the BuildKit cache is pruned to `cache_gb`.

## From an app: the jobs bridge

`c0mpute jobs serve` (see `c0mpute jobs serve --help`) accepts container
jobs over HTTP:

```sh
curl -sH "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" --data-binary @ctx.tgz \
  http://127.0.0.1:8788/v1/blobs                      # {"blob":"<id>","size":...}
curl -sH "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" -d '{
  "workload": "container.run",
  "allow_peers": ["<builder peer id>"],
  "spec": {"context_blob": "<id>", "build": {"dockerfile": "Dockerfile"}, "timeout_secs": 1800}
}' http://127.0.0.1:8788/v1/jobs                      # result + image_url
curl -sH "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" -o image.tar.gz http://127.0.0.1:8788/v1/blobs/<image id>
```

A run is the same with `image` or `build`, `cmd`, `input_blob`, `outputs`;
the response lists `outputs[].url`. Blobs stay for an hour.

## From an agent: MCP

`c0mpute apps mcp` has `container_build` (dir, dockerfile, build_args,
target, output, load, tags, peers) and `container_run` (image or build, cmd,
input, outputs, dest, env, peers).

## Deploying with it (dev2 sites)

The dev2 `deploy-app.sh` takes `REMOTE_BUILD=c0mpute` in `deploy.env`: it
packs the checked-out revision with `git archive`, passes the public build
args the compose file names, runs `c0mpute build --load -t <compose image>`,
then the usual `compose up --no-build`, health check and rollback.
`C0MPUTE_BUILDERS` there names the builders. If the remote build fails, the
deploy stops before touching the running stack, like a local build failure.
