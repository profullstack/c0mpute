# c0mpute.com v1 Augmentation PRD

> **Superseded where it conflicts with the v2 direction.** c0mpute v2 makes
> the network a protocol and open marketplace rather than a fleet plus
> plugins, and no hosted service may be authoritative for protocol
> correctness (DIP-0024). Where this document assumes a coordinator, a
> central scheduler, or CoinPay as the identity layer, the v2 direction and
> `docs/protocol/` win. Everything else here — the module model, the
> workload verticals, the pricing work — carries forward unchanged.

## Status

Draft augmentation PRD for steering the existing p2p compute work toward the new c0mpute.com direction before implementation goes too far in the wrong direction.

This supersedes the v1 architecture in [`PRD.md`](PRD.md). The original PRD's
domain-specific content (FFmpeg integration, transcoding pricing, Reed-Solomon
chunking, libp2p protocol IDs) carries forward as the **transcode module**
inside c0mpute, not as a standalone product.

## Product Summary

c0mpute.com is a generic decentralized compute network for paid workloads. The network coordinates buyers, workers, validators, payments, reputation, and workload execution across a p2p compute marketplace.

The v1 product should focus on two practical workload categories:

1. Audio/video transcoding with FFmpeg
2. AI inference jobs through Infernet Protocol

CoinPay DID should be used as the identity, trust, payments, escrow, receipts, staking, and reputation layer for the network.

Infernet Protocol should remain the AI/inference-specific protocol and CLI, but it should sit under the broader c0mpute network umbrella.

## One-Liner

c0mpute.com is a decentralized compute network for paid FFmpeg transcoding and AI inference jobs, powered by CoinPay DID trust, escrow, and worker reputation.

## Brand and Product Architecture

```txt
c0mpute.com
  Generic p2p compute network and marketplace

CoinPay
  DID identity, wallets, escrow, payments, receipts, staking, reputation

Infernet Protocol
  AI inference workload protocol and runtime running on c0mpute
```

Recommended positioning:

```txt
c0mpute.com is the generic p2p compute network.
Infernet Protocol is the AI inference workload protocol.
CoinPay DID is the trust and payments layer.
```

## Goals

- Rebrand the generic p2p compute network around c0mpute.com.
- Keep Infernet Protocol focused on inference instead of making it carry the entire generic compute brand.
- Launch v1 with two useful workload categories: FFmpeg transcoding and Infernet inference.
- Use CoinPay DID as the portable trust passport for workers, buyers, validators, and organizations.
- Use CoinPay escrow and receipts for payment fairness.
- Make worker onboarding simple through one canonical c0mpute installer.
- Install the full v1 CLI stack by default: c0mpute, coinpay, and infernet.
- Provide a developer-friendly CLI and job manifest format.
- Avoid overbuilding exotic trust systems before the first paid workloads work.

## Non-Goals for v1

- Do not attempt large-scale distributed training of mega models in v1.
- Do not try to become a full AWS replacement.
- Do not support arbitrary untrusted shell execution as the default public workload mode.
- Do not require zk proofs for ordinary jobs.
- Do not make every worker complete KYC before running low-value public jobs.
- Do not expose private customer secrets to untrusted workers.
- Do not make Infernet responsible for non-AI generic compute concerns.

## V1 Workloads

### 1. FFmpeg Audio/Video Transcoding

FFmpeg transcoding should be the default practical v1 workload because it is useful, commercial, sandboxable, retryable, and easier to validate than arbitrary compute.

Example commands:

```bash
c0mpute transcode input.mov --preset web-1080p
c0mpute transcode input.mov --preset hls
c0mpute ffmpeg transcode input.mov --to mp4
```

Suggested presets:

```txt
audio-mp3
audio-aac
audio-opus
video-720p
video-1080p
video-4k
hls
dash
thumbnail
gif
extract-audio
normalize-audio
```

Validation strategies:

```txt
ffprobe metadata validation
codec checks
resolution checks
duration checks
bitrate bounds
file size sanity checks
segment hash checks
output schema checks
optional duplicate validation for high-value jobs
```

### 2. Infernet Protocol Inference

Infernet Protocol should power AI inference jobs on top of the c0mpute network.

Example commands:

```bash
c0mpute infer prompts.jsonl --model qwen
c0mpute infer prompts.jsonl --model llama-3.1-8b --max-price 0.25
infernet run prompts.jsonl --model qwen --network c0mpute
```

Inference validation strategies:

```txt
model hash verification
runtime image hash verification
prompt/input hash
output schema validation
token count sanity checks
random duplicate sampling
judge validation for selected jobs
customer acceptance window
attested compute later for premium/private jobs
```

## CLI Toolchain Strategy

The c0mpute installer should be the canonical installer for the full v1 decentralized compute stack.

Default install should include:

```txt
c0mpute  - generic p2p compute network CLI
coinpay  - DID, wallet, escrow, payments, receipts, staking, reputation
infernet - AI inference workload CLI/protocol
```

The user should be able to install everything with one command:

```bash
curl -fsSL https://c0mpute.com/install.sh | sh
```

After installation, these binaries should be available on `$PATH`:

```bash
c0mpute --version
coinpay --version
infernet --version
```

## CLI Responsibilities

### c0mpute CLI

The c0mpute CLI is the umbrella command for the generic compute network.

Responsibilities:

```txt
worker registration
worker runtime
job submission
job scheduling interface
job status/logs
FFmpeg workload commands
capability discovery
network configuration
validator interactions
doctor checks
high-level user workflows
```

Example commands:

```bash
c0mpute doctor
c0mpute worker register
c0mpute worker start
c0mpute worker status
c0mpute job submit job.json
c0mpute job status <job-id>
c0mpute job logs <job-id>
c0mpute transcode input.mov --preset hls
c0mpute infer prompts.jsonl --model qwen
c0mpute trust inspect did:coinpay:worker:abc123
```

### CoinPay CLI

The CoinPay CLI handles identity, wallet, escrow, payments, receipts, and reputation primitives.

Responsibilities:

```txt
DID creation
wallet linking
wallet status
escrow funding
escrow release
payment receipts
worker payout information
staking and slashing later
reputation events
credential inspection
```

Example commands:

```bash
coinpay did create
coinpay did status
coinpay wallet status
coinpay escrow status
coinpay receipts list
coinpay reputation inspect did:coinpay:worker:abc123
```

### Infernet CLI

The Infernet CLI handles AI inference-specific workflows.

Responsibilities:

```txt
model execution
batch inference jobs
model/runtime configuration
inference runner integration
model registry integration
AI-specific validation options
network integration with c0mpute
```

Example commands:

```bash
infernet doctor
infernet run prompts.jsonl --model qwen --network c0mpute
infernet models list
infernet benchmark --model qwen
```

## CLI Relationship

c0mpute should provide the simplest user experience and may call CoinPay and Infernet functionality internally.

A user should not need to understand all three tools before submitting a simple job.

Example high-level command:

```bash
c0mpute infer prompts.jsonl --model qwen
```

Internally, c0mpute can delegate or integrate with:

```bash
coinpay did status
coinpay escrow create
infernet run prompts.jsonl --model qwen --network c0mpute
```

But advanced users should still be able to use each CLI independently.

## Installer Requirements

### Default Installer

The default installer should install or update the complete v1 CLI stack.

```bash
curl -fsSL https://c0mpute.com/install.sh | sh
```

The installer must:

1. Detect OS and architecture.
2. Install or update `c0mpute`.
3. Install or update `coinpay`.
4. Install or update `infernet`.
5. Verify all three binaries are on `$PATH`.
6. Print installed versions.
7. Run basic doctor checks where possible.
8. Print next-step commands.
9. Fail clearly with actionable error messages.
10. Avoid silently modifying shell config unless required and disclosed.

### Installer Modes

Support optional flags:

```bash
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --minimal
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --no-coinpay
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --no-infernet
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --worker
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --developer
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --force
```

Flag behavior:

```txt
--minimal       Install only c0mpute
--no-coinpay    Skip CoinPay CLI installation
--no-infernet   Skip Infernet CLI installation
--worker        Install worker-focused dependencies/checks
--developer     Install developer/test tools and verbose diagnostics
--force         Reinstall even if binaries already exist
```

### Installer Output UX

After a successful default install, output should look roughly like this:

```txt
c0mpute installed:  v0.1.x
coinpay installed:  v0.1.x
infernet installed: v0.1.x

Next steps:
  coinpay did create
  c0mpute worker register
  c0mpute doctor
  c0mpute worker start
```

### Doctor Command

`c0mpute doctor` should check the full stack.

Example output:

```txt
c0mpute: installed
coinpay: installed
infernet: installed
docker: installed
ffmpeg: installed
wallet: connected
did: configured
worker: not registered
network: reachable
```

Minimum checks:

```txt
c0mpute binary exists
coinpay binary exists
infernet binary exists
Docker or supported sandbox runtime exists
FFmpeg exists or can be pulled through runner image
network API reachable
CoinPay DID status
wallet status
worker registration status
Infernet runtime status
```

## Worker Onboarding Flow

Recommended happy path:

```bash
curl -fsSL https://c0mpute.com/install.sh | sh
coinpay did create
c0mpute worker register
c0mpute worker start
```

Expected behavior:

1. User installs the full CLI stack.
2. User creates or imports a CoinPay DID.
3. User links or creates a wallet.
4. User registers as a c0mpute worker.
5. Worker advertises capabilities.
6. Worker starts polling or listening for jobs.
7. Completed jobs generate signed receipts.
8. CoinPay escrow releases payment after validation.
9. Worker reputation updates against the worker DID.

## Buyer Job Submission Flow

Recommended happy path:

```bash
curl -fsSL https://c0mpute.com/install.sh | sh
coinpay did create
c0mpute transcode input.mov --preset web-1080p --max-price 1.25
```

For inference:

```bash
c0mpute infer prompts.jsonl --model qwen --max-price 5.00
```

Expected behavior:

1. Buyer submits job.
2. CLI creates or references signed job manifest.
3. CLI funds or authorizes escrow through CoinPay.
4. Scheduler matches job to eligible worker.
5. Worker runs sandboxed workload.
6. Validator checks output.
7. Output is made available to buyer.
8. CoinPay releases payment.
9. Receipt updates buyer, worker, and validator histories.

## CoinPay DID Trust Model

CoinPay DID should act as the identity spine of the c0mpute network.

Each actor should have a DID:

```txt
did:coinpay:buyer:abc123
did:coinpay:worker:def456
did:coinpay:validator:ghi789
did:coinpay:org:jkl012
```

The DID should bind together:

```txt
wallet addresses
payment history
worker reputation
validator reputation
staked collateral
optional KYC/KYB status
hardware attestations
signed job receipts
slashing history
dispute history
organization identity
```

DID can prove:

```txt
same worker identity over time
control of key/wallet
job acceptance signature
result submission signature
payment receipt history
completed jobs
success/failure rate
stake amount
optional credential status
```

DID cannot prove by itself:

```txt
output correctness
worker honesty
GPU authenticity
private data protection
model actually used
runtime integrity
```

Therefore DID must be paired with sandboxing, validation, escrow, reputation, staking, and eventually attestation.

## Trust Tiers

Recommended v1 trust tiers:

```txt
cheap:
  DID required
  wallet linked
  sandbox required
  reputation filter
  low max job value

standard:
  DID required
  signed receipts
  validator checks
  payout delay for new workers

verified:
  DID required
  stake required
  redundant validation or stronger checks
  slashing enabled

private:
  DID required
  encrypted inputs
  attestation credential required
  restricted logs
  premium workers only

enterprise:
  org DID
  KYC/KYB credential
  allowlisted workers
  audit logs
  SLA/support later
```

## Job Manifest Format

### FFmpeg Job Example

```json
{
  "version": "0.1",
  "network": "c0mpute",
  "type": "ffmpeg.transcode",
  "buyer": "did:coinpay:buyer:abc",
  "input": {
    "uri": "https://signed-url/input.mov",
    "sha256": "sha256:..."
  },
  "runtime": {
    "image": "ghcr.io/c0mpute/ffmpeg-runner@sha256:...",
    "command": ["transcode", "--preset", "web-1080p"]
  },
  "output": {
    "format": "mp4",
    "requirements": {
      "videoCodec": "h264",
      "audioCodec": "aac",
      "maxWidth": 1920,
      "maxHeight": 1080
    }
  },
  "payment": {
    "escrow": "coinpay",
    "maxPriceUsd": 1.25
  },
  "validation": {
    "mode": "ffprobe",
    "checks": ["duration", "codec", "resolution", "bitrate"]
  }
}
```

### Infernet Job Example

```json
{
  "version": "0.1",
  "network": "c0mpute",
  "type": "infernet.inference",
  "buyer": "did:coinpay:buyer:abc",
  "model": {
    "name": "qwen",
    "hash": "sha256:..."
  },
  "input": {
    "uri": "https://signed-url/prompts.jsonl",
    "sha256": "sha256:..."
  },
  "runtime": {
    "image": "ghcr.io/c0mpute/infernet-runner@sha256:..."
  },
  "payment": {
    "escrow": "coinpay",
    "maxPriceUsd": 5.00
  },
  "validation": {
    "mode": "schema_and_spotcheck",
    "duplicateSampleRate": 0.05
  }
}
```

## Signed Job Receipt

Every completed job should generate a signed receipt.

```json
{
  "jobId": "job_123",
  "network": "c0mpute",
  "type": "ffmpeg.transcode",
  "buyer": "did:coinpay:buyer:abc",
  "worker": "did:coinpay:worker:def",
  "validator": "did:coinpay:validator:ghi",
  "inputHash": "sha256:...",
  "outputHash": "sha256:...",
  "runtimeImage": "sha256:...",
  "price": "1.25",
  "currency": "USDC",
  "status": "accepted",
  "workerSignature": "...",
  "validatorSignature": "...",
  "paidAt": "2026-05-03T12:34:00Z"
}
```

Receipts should update:

```txt
worker reputation
validator reputation
buyer history
payment history
dispute history, if applicable
```

## Security and Sandboxing Requirements

Default public jobs must run in isolated environments.

Minimum sandbox expectations:

```txt
containerized or WASM execution
no privileged host access
no arbitrary host mounts
resource limits
read-only filesystem where possible
restricted network egress where possible
signed runner images
runtime image digest pinning
input/output path isolation
job timeout enforcement
```

Public network should prefer:

```txt
OCI image digest + command + input blob + output path
```

Public network should avoid:

```txt
curl random-script.sh | bash
privileged Docker containers
host filesystem mounts
long-lived buyer secrets
raw API keys exposed to workers
```

## Payments and Escrow

CoinPay should handle job payment flows.

Basic flow:

```txt
buyer creates job
buyer funds escrow
worker accepts job
worker runs job
worker submits output and signed receipt
validator validates output
escrow releases payment
reputation updates
```

For v1:

```txt
low-value jobs can use reputation and payout delay
higher-value jobs may require stake
private or enterprise jobs may require verified workers
slashing can be added progressively
```

## Reputation Requirements

Track reputation by CoinPay DID.

Suggested metrics:

```txt
completed jobs
failed jobs
timeout rate
dispute rate
validation success rate
average validation score
hardware consistency
attestation support
uptime
bandwidth
latency
stake history
slashing history
customer ratings
validator quality
```

Expose useful badges instead of only one opaque trust score:

```txt
431 completed jobs
98.7% validation success
$500 staked
KYC verified, optional
H100 attested, optional
No slashing events in 90 days
```

## Scheduler Requirements

The scheduler should match jobs to workers based on:

```txt
workload type
hardware capability
price
worker reputation
trust tier
geographic/latency preference, optional
availability
success rate
stake/credential requirements
```

V1 capabilities to advertise:

```txt
cpu cores
ram
gpu type
gpu vram
ffmpeg support
hardware encoder support
infernet support
available models
network bandwidth
sandbox runtime
attestation support, later
```

## MVP API Concepts

Initial internal services/modules:

```txt
job registry
worker registry
scheduler
validator service
receipt service
CoinPay escrow integration
CoinPay DID/reputation integration
artifact storage adapter
runner image registry
```

Possible API endpoints:

```txt
POST /jobs
GET /jobs/:id
POST /jobs/:id/cancel
POST /workers/register
POST /workers/heartbeat
GET /workers/:did
POST /receipts
GET /receipts/:jobId
POST /validators/result
GET /trust/:did
```

## Success Metrics

V1 should be judged by practical usage, not theoretical decentralization.

Key metrics:

```txt
time from install to first successful job
number of registered workers
number of successful FFmpeg jobs
number of successful inference jobs
job success rate
job timeout rate
validation failure rate
average job completion time
average worker earnings
average buyer cost vs cloud alternative
repeat buyer usage
repeat worker usage
```

## Implementation Priorities

### Phase 1: Reorientation

- Rename/gate generic compute architecture around c0mpute.com.
- Keep Infernet as inference-specific.
- Define c0mpute, coinpay, and infernet CLI boundaries.
- Update docs, README, installer assumptions, and diagrams.

### Phase 2: Installer and CLI Foundation

- Build canonical c0mpute installer.
- Install c0mpute, coinpay, and infernet by default.
- Add installer flags.
- Add `c0mpute doctor` full-stack checks.
- Add worker registration flow.

### Phase 3: FFmpeg Workload MVP

- Add FFmpeg runner image.
- Add transcode command.
- Add job manifest generation.
- Add ffprobe validation.
- Add signed receipt generation.
- Add CoinPay escrow integration stub or live path.

### Phase 4: Infernet Workload MVP

- Add Infernet runner integration.
- Add `c0mpute infer` command.
- Add model/runtime hash fields.
- Add basic schema/spotcheck validation.
- Add inference receipts.

### Phase 5: Trust and Reputation

- Bind jobs to CoinPay DID.
- Add reputation updates from receipts.
- Add worker trust badges.
- Add payout delay for new workers.
- Add stake/slashing design for later.

## Open Questions

- Should c0mpute, coinpay, and infernet be separate repos, monorepo packages, or separate binaries from one monorepo?
- Should c0mpute shell out to coinpay/infernet CLIs or use shared SDK libraries?
- What is the first supported payment currency for escrow?
- Where are input/output artifacts stored in v1?
- Should workers pull jobs from an API or receive jobs through p2p discovery first?
- Should FFmpeg jobs be chunked in v1 or handled as whole-file jobs first?
- Which inference runtime is first-class in v1: Ollama, vLLM, llama.cpp, custom Infernet runner, or multiple?
- What is the minimum viable validation policy for nondeterministic LLM output?

## Final Direction

The project should move forward with this hierarchy:

```txt
c0mpute.com = generic decentralized compute network
CoinPay DID = trust, identity, payments, escrow, reputation
Infernet Protocol = AI inference workload layer
FFmpeg = first practical non-AI workload
```

The default c0mpute installer should install the complete v1 stack:

```txt
c0mpute + coinpay + infernet
```

This gives the project a cleaner architecture, a practical first paid workload, a strong AI story, and a reusable trust/payment layer across future Profullstack marketplaces.
