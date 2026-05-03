# Storage TCO scenarios

Companion to [`storage-pricing.csv`](storage-pricing.csv). Same numbers,
worked through real workloads so the cost shape is obvious.

## Scenario 1: Video on demand library (1 TB, modest viewership)

A creator hosting 1 TB of transcoded video, with 100 GB streamed to
viewers per month.

| Provider | Storage | Egress | Monthly TCO |
|---|---|---|---|
| AWS S3 Standard | $23.00 | $9.00 | **$32.00** |
| Google Cloud Standard | $20.00 | $12.00 | $32.00 |
| Cloudflare R2 | $15.00 | $0.00 | **$15.00** |
| Wasabi | $6.90 | $0.00 (within cap) | $6.90 |
| Backblaze B2 | $6.00 | $1.00 | $7.00 |
| **c0mpute (internet)** | **$8.00** | **$0.50** | **$8.50** |
| **c0mpute (served to other c0mpute jobs)** | **$8.00** | **$0.00** | **$8.00** |
| Storj | $4.00 | $0.70 | $4.70 |
| Hetzner Object Storage | $5.80 | $0.00 | $5.80 |
| Filecoin (deal) | $0.10 | varies (slow) | $0.10+ |

**Winners:** Hetzner Object, B2, Wasabi for cheap-and-fast.
R2 wins TCO when egress is the dominant cost.
c0mpute is in the same league as B2 / Wasabi for raw cost,
**price-equivalent to B2 with structural egress savings** when the
viewers/processors are inside the c0mpute network.

## Scenario 2: HLS streaming library (1 TB, heavy viewership)

Same 1 TB but 10 TB streamed per month (popular content, public
viewership).

| Provider | Storage | Egress | Monthly TCO |
|---|---|---|---|
| AWS S3 Standard | $23.00 | $900.00 | **$923.00** |
| Google Cloud Standard | $20.00 | $1,200.00 | $1,220.00 |
| Cloudflare R2 | $15.00 | $0.00 | **$15.00** ← winner |
| Wasabi | $6.90 | $0 first 1 TB then $0.06/GB | ~$540 |
| Backblaze B2 | $6.00 | $97.00 (first 18 GB free) | ~$103 |
| **c0mpute (internet egress)** | **$8.00** | **$50.00** | **$58.00** |
| **c0mpute (served to other c0mpute jobs)** | **$8.00** | **$0.00** | **$8.00** |
| Storj | $4.00 | $70.00 | $74.00 |
| Hetzner Object Storage | $5.80 | $0.00 | $5.80 ← but new product |
| Pinata Picnic | $400 | included | $400 |

**Winners at heavy viewership:** R2 (zero egress), Hetzner. AWS / GCS
become absurdly expensive.
c0mpute at $0.005/GB internet egress sits ~6× more than R2 but ~9×
cheaper than B2's effective rate. **Still 16× cheaper than S3.**

For streaming-to-other-c0mpute-jobs (e.g., AI inference batch jobs
fetching videos for processing), the $0 internal egress is the same
as R2 — same cost shape, plus c0mpute also runs the compute.

## Scenario 3: One-shot upload of training corpus (10 TB write, no read)

A research team uploading 10 TB of training data once, never reading
it back from this storage (compute happens on-network, dataset stays
local to workers).

| Provider | Storage / mo | One-time write | First month total |
|---|---|---|---|
| AWS S3 Standard | $230.00 | $0 ingress | $230 |
| Cloudflare R2 | $150.00 | $0 ingress | $150 |
| Backblaze B2 | $60.00 | $0 ingress | $60 |
| **c0mpute** | **$80.00** | **$0 ingress** | **$80** |
| Storj | $40.00 | $0 ingress | $40 |
| Hetzner Object | $58.00 | $0 ingress | $58 |
| Filecoin | $1.00 | deal-making cost ~ varies | $1+ |

Uploads are free everywhere except Arweave (pay-once permanent) and
Filecoin (deal-making cost). The cost is in the storage tail.

## Scenario 4: Cold archive (1 TB, almost never read)

Backup archive, expected reads <1 GB/year.

| Provider | Storage / mo | Annual TCO |
|---|---|---|
| AWS S3 Glacier Deep Archive | $0.99 | ~$12 |
| Google Cloud Archive | $1.20 | ~$15 |
| Azure Blob Archive | $0.99 | ~$12 |
| **c0mpute** | **$8.00** | **~$96** |
| Storj | $4.00 | ~$48 |
| Filecoin | $0.10 | ~$1.20 (slow retrieval) |

**c0mpute is bad at cold archive.** Filecoin or AWS Glacier Deep
Archive win; we don't have a cold tier in the design.

## Per-request costs sanity check (HLS streaming)

A 1-hour HLS stream typically issues ~720 GET requests (6-second
segments). Per-1k-GET pricing:

| Provider | $/M GETs | per 1-hour stream |
|---|---|---|
| AWS S3 | $0.40 | $0.000288 |
| GCS | $0.40 | $0.000288 |
| Azure | $5.00 | $0.0036 |
| **Cloudflare R2** | **$0.36** | **$0.000259** |
| **B2** | **$4.00** | **$0.00288** |
| **c0mpute** | included | $0 |

Per-request charges round to "noise" in monthly bills unless you're
serving billions of segments. R2 wins on raw rate but it doesn't
move the needle for normal traffic.

## Where c0mpute actually wins

The TCO tables show c0mpute is **competitively priced** but not the
cheapest on raw $/GB. The actual win is when:

1. **Compute and storage are co-located.** c0mpute jobs reading
   c0mpute-stored data pay $0 egress. Other providers' $0-egress
   tiers (R2, Wasabi, Hetzner) still require the data to leave their
   network into the worker, even if they don't bill for it directly.
2. **Workload is GPU-batch + read-once data.** AI inference with
   one-shot input reads, transcode pipelines — the egress to the
   compute is internal in c0mpute's case.
3. **Sovereignty / E2E encryption matters.** Customer-held keys for
   stored data; the network never sees plaintext.
4. **No commitment.** No 90-day floors (Wasabi), no annual contracts
   (Hetzner), no token deposits (Filecoin / Sia / Arweave).

We don't claim to be the cheapest at $/GB. We claim to be **the
cheapest end-to-end for c0mpute-resident workloads**.
