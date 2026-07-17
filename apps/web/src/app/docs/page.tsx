import Link from "next/link";
import { CodeBlock as Code } from "@/components/CodeBlock";

export const metadata = {
  title: "docs — c0mpute",
  description: "CLI reference and cookbook for c0mpute: install, identity, workers, transcode jobs, AI inference, reputation, plugins, and health checks.",
  alternates: { canonical: "https://c0mpute.com/docs" },
};

export default function DocsPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-12">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">docs</h1>
        <p className="comment">// the cli is the surface; here's the cookbook</p>
      </header>

      <Section h="install" title="[ install ]">
        <Code>
          {`curl -fsSL https://c0mpute.com/install.sh | sh`}
        </Code>
        <P>
          Drops three binaries into <code>~/.c0mpute/bin</code>:{" "}
          <code>c0mpute</code>, <code>coinpay</code>, <code>infernet</code>.
          Self-upgrades on its own schedule.
        </P>
        <P>
          Variants:
        </P>
        <Code>
{`# c0mpute only (no coinpay, no infernet)
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --minimal

# Worker box: also installs Docker + FFmpeg readiness checks
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --worker

# Reinstall over an existing install
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --force`}
        </Code>
      </Section>

      <Section h="identity" title="[ identity & wallet ]">
        <P>
          Every actor on the network has a CoinPay DID. Sign in to coinpay;{" "}
          <code>c0mpute worker register</code> then sets up the DID
          automatically (uses your existing one or creates it).
        </P>
        <Code>
{`c0mpute coinpay login                       # sign in (links your payable DID)
c0mpute coinpay reputation did setup        # use existing DID or create one
c0mpute coinpay reputation did me           # show active DID + key fingerprint
c0mpute coinpay wallet status               # linked addresses + balances`}
        </Code>
      </Section>

      <Section h="worker" title="[ run a worker ]">
        <P>
          Register, then start. <code>--gpu</code> opts into transcode +
          inference roles; storage / gateway roles run on any box.
        </P>
        <Code>
{`c0mpute worker register
c0mpute worker start \\
  --roles storage,transcode,gateway,verifier \\
  --storage 500GB \\
  --gpu

# Inspect status / restart / stop
c0mpute worker status
c0mpute worker stop`}
        </Code>
      </Section>

      <Section h="networking" title="[ networking &amp; firewall ]">
        <P>
          To be dialable by other nodes — <strong>required</strong> for a
          bootstrap seed, recommended for a worker so it can receive jobs —
          open your libp2p p2p port. Pin it with{" "}
          <code>C0MPUTE_P2P_PORT=&lt;port&gt;</code> (otherwise a random port is
          used); <code>46337</code> is the convention. If your host has a{" "}
          <em>cloud</em> firewall (DigitalOcean, AWS SG, GCP), open the port
          there <strong>and</strong> in the host firewall — a cloud firewall
          drops traffic before ufw ever sees it.
        </P>
        <Code>
{`# host firewall (ufw)
sudo ufw allow 46337/tcp

# DigitalOcean cloud firewall (doctl)
doctl compute firewall add-rules <firewall-id> \\
  --inbound-rules "protocol:tcp,ports:46337,address:0.0.0.0/0,address:::/0"

# Railway: no host firewall — add a TCP Proxy (Settings -> Networking),
# or (per DIP-0010) run the seed on a droplet with a stable public IP.

# verify from OUTSIDE the box (not localhost):
#   https://check-host.net/check-tcp?host=<public-ip>:46337`}
        </Code>
      </Section>

      <Section h="transcode" title="[ submit a transcode job ]">
        <P>
          The transcode plugin handles FFmpeg workloads — H.264 / HEVC /
          AV1 with hardware acceleration where available. Validation is
          done via ffprobe + (optionally) VMAF.
        </P>
        <Code>
{`# 1080p H.264 four-rendition HLS bundle
c0mpute transcode submit input.mov \\
  --preset hls \\
  --max-price 1.25

# AV1 4K transcode capped at $5
c0mpute transcode submit input.mp4 \\
  --preset video-4k \\
  --max-price 5.00

# List available presets
c0mpute transcode preset list`}
        </Code>
        <P>
          Job manifest schema (sent to workers, signed by your DID):
        </P>
        <Code>
{`{
  "version": "0.1",
  "network": "c0mpute",
  "type": "ffmpeg.transcode",
  "buyer": "did:coinpay:buyer:abc",
  "input": {
    "uri": "https://your-storage/input.mov",
    "sha256": "sha256:..."
  },
  "runtime": {
    "image": "ghcr.io/c0mpute/ffmpeg-runner@sha256:...",
    "command": ["transcode", "--preset", "hls"]
  },
  "output": {
    "format": "hls",
    "requirements": {
      "videoCodec": "h264",
      "audioCodec": "aac",
      "maxWidth": 1920,
      "maxHeight": 1080
    }
  },
  "payment": { "escrow": "coinpay", "maxPriceUsd": 1.25 },
  "validation": {
    "mode": "ffprobe",
    "checks": ["duration", "codec", "resolution", "bitrate"]
  }
}`}
        </Code>
      </Section>

      <Section h="infer" title="[ run AI inference ]">
        <P>
          The infernet plugin runs LLM and other ML inference on c0mpute
          workers' GPUs. Validation uses output-schema checks +
          spot-check duplicate execution.
        </P>
        <Code>
{`# Simple batch with default model
c0mpute infernet run prompts.jsonl \\
  --model qwen \\
  --max-price 5.00

# Pin a specific model + runtime image hash for reproducible runs
c0mpute infernet run prompts.jsonl \\
  --model llama-3.1-8b \\
  --max-price 0.25

# List models the network advertises
c0mpute infernet models list

# Benchmark a model against the network's workers
c0mpute infernet benchmark --model qwen`}
        </Code>
      </Section>

      <Section h="jobs" title="[ track jobs ]">
        <Code>
{`c0mpute job status <job-id>          # one-shot status
c0mpute job logs <job-id> --follow   # tail logs
c0mpute job cancel <job-id>          # cancel queued/running
c0mpute tui                          # interactive worker / job dashboard`}
        </Code>
      </Section>

      <Section h="trust" title="[ inspect trust / reputation ]">
        <P>
          Every worker, validator, buyer, and org has badges anchored to
          their DID. No opaque trust score; surface the metrics.
        </P>
        <Code>
{`c0mpute coinpay reputation inspect did:coinpay:worker:def456`}
        </Code>
        <P>Sample output:</P>
        <Code>
{`did: did:coinpay:worker:def456
  431 completed jobs
  98.7% validation success
  $500 staked
  KYC verified (optional)
  H100 attested (optional)
  No slashing events in 90 days`}
        </Code>
      </Section>

      <Section h="plugins" title="[ install third-party plugins ]">
        <Code>
{`# Install from a plugin's signed install.sh
c0mpute plugin install https://example.com/my-plugin/install.sh

# Install by id from the c0mpute marketplace
c0mpute plugin install my-plugin

# List installed
c0mpute plugin list

# Disable / re-enable / remove
c0mpute plugin disable my-plugin
c0mpute plugin enable my-plugin
c0mpute plugin uninstall my-plugin`}
        </Code>
        <P>
          Browse what's available at{" "}
          <Link href="/plugins">/plugins</Link>.
        </P>
      </Section>

      <Section h="health" title="[ self-check ]">
        <P>
          <code>c0mpute doctor</code> walks the full stack — binary
          presence, FFmpeg, Docker, GPU drivers, disk, clock drift,
          coordinator reach, and the known-issues feed.
        </P>
        <Code>
{`c0mpute doctor                         # full report
c0mpute doctor --fix                   # auto-apply known remediations
c0mpute doctor --report                # send anonymized telemetry`}
        </Code>
      </Section>

      <Section h="design" title="[ design proposals ]">
        <P>
          Architecture decisions live in the{" "}
          <a href="https://github.com/profullstack/c0mpute/tree/master/dips">
            <code>dips/</code>
          </a>{" "}
          directory. Highlights:
        </P>
        <ul className="space-y-1 pl-5 text-sm leading-6">
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0005-c0mpute-rebrand.md">
              DIP-0005
            </a>{" "}
            — c0mpute rebrand, three-CLI architecture
          </li>
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0006-module-model.md">
              DIP-0006
            </a>{" "}
            — module manifest + dispatch model
          </li>
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0007-coinpay-did-identity.md">
              DIP-0007
            </a>{" "}
            — CoinPay DID as the identity layer
          </li>
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0008-ui-strategy.md">
              DIP-0008
            </a>{" "}
            — CLI-first UI strategy
          </li>
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0010-bootstrap-seed-nodes.md">
              DIP-0010
            </a>{" "}
            — libp2p bootstrap seed nodes
          </li>
          <li>
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0011-no-central-backend.md">
              DIP-0011
            </a>{" "}
            — no central backend
          </li>
        </ul>
      </Section>

      <p className="text-xs text-[var(--color-dim)] rule pt-6">
        → <Link href="/getting-started">getting-started</Link> ·{" "}
        <Link href="/plugins">plugins</Link> ·{" "}
        <Link href="/contact">contact</Link>
      </p>
    </div>
  );
}

function Section({
  h,
  title,
  children,
}: {
  h: string;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section id={h} className="space-y-3">
      <h2 className="text-lg accent">{title}</h2>
      <div className="pl-5 space-y-3">{children}</div>
    </section>
  );
}

function P({ children }: { children: React.ReactNode }) {
  return <p className="text-sm leading-6">{children}</p>;
}

// Code = re-export of CodeBlock from "@/components/CodeBlock" via the
// import alias above. Kept as `Code` here for diff-quietness — the page
// has many <Code>...</Code> uses.
