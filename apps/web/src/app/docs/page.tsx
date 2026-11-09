import Link from "next/link";
import { CodeBlock as Code } from "@/components/CodeBlock";

export const metadata = {
  title: "docs — c0mpute",
  description: "CLI reference and cookbook for c0mpute: install, identity, workers, transcode jobs, AI inference, reputation, plugins, WebMCP tools for AI agents, and health checks.",
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

      <Section h="duplicate-workers" title="[ troubleshooting: one identity, two workers ]">
        <P>
          A node is its identity (<code>~/.config/c0mpute/identity.key</code>).
          Two processes running one identity bid under one peer id with
          different keys, and the one the buyer did not pick used to fail the
          job. Typical causes: a system unit (<code>/etc/systemd/system</code>,{" "}
          <code>User=you</code>) next to your user unit, or an identity dir
          copied to a second machine.
        </P>
        <P>What c0mpute does about it now:</P>
        <ul className="text-sm leading-6 list-disc pl-5 space-y-1">
          <li>
            Same machine: the newest <code>worker start</code> wins. The running
            worker stops bidding, finishes its jobs (up to 10 min) and exits with
            code 90; one that does not answer gets SIGTERM, then SIGKILL, after
            its pid, start time and binary are re-checked.
          </li>
          <li>
            Every unit c0mpute installs has{" "}
            <code>RestartPreventExitStatus=90 91</code>, so systemd does not
            restart the replaced worker. Older units get a drop-in from the
            running worker (your own user units; system units only as root,
            otherwise the log prints the <code>sudo</code> command). The new
            worker disables the replaced unit when it may.
          </li>
          <li>
            Two machines: buyers keep the newer process&apos;s bid and name its
            instance on the accept. The other one answers &quot;not mine&quot;, the
            buyer re-awards the job to the next bidder at once (no failed job,
            no bench), and the older twin stops bidding while the newer one runs.{" "}
            <Link href="/nodes">/nodes</Link> marks the node{" "}
            <em>duplicate identity</em>.
          </li>
        </ul>
        <Code>
{`c0mpute doctor          # every worker process, unit and identity lock here:
                        # duplicates, stale locks, wedged workers, old binaries
c0mpute doctor --fix    # disable the extra unit (prints sudo for system units)
c0mpute worker install  # one user unit; refuses if another unit runs a worker

# Remove a duplicate system unit by hand:
sudo systemctl disable --now c0mpute-worker.service

# A copied identity: give the copy its own identity before starting it
rm ~/.config/c0mpute/identity.key && c0mpute worker start`}
        </Code>
        <P>
          Exit codes: <code>90</code> replaced by a newer start,{" "}
          <code>91</code> a duplicate that stood down. Neither is a crash.
        </P>
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
{`# Run a prompt on the network (stdin works too)
c0mpute infernet chat --model qwen2.5:7b "summarise this"
cat prompt.txt | c0mpute infernet chat --model qwen2.5:7b

# How many live nodes serve each model
c0mpute pool
c0mpute pool qwen

# The models one node advertises (node_id or row id)
c0mpute infernet model list --node <id>

# Models pulled on this machine
c0mpute infernet model list

# Hardware-aware model picks for writing, research, or coding
c0mpute infernet model recommend --use-case marketing
c0mpute infernet model recommend --use-case research
c0mpute infernet model recommend --use-case coding`}
        </Code>
      </Section>

      <Section h="temporal" title="[ temporal policies ]">
        <P>
          Train and serve small liquid neural networks (CfC, LTC) and GRU
          baselines that predict the next UI action, on workers you trust.
          Full guide: <a href="/temporal">c0mpute.com/temporal</a>.
        </P>
        <Code>
{`c0mpute infernet temporal serve --train
c0mpute infernet temporal train --dataset bundle.json --output ./model --family cfc --baseline gru --worker <peer-id> --yes
c0mpute infernet temporal predict --session s1 --model sha256:<hash> --input step.json`}
        </Code>
      </Section>

      <Section h="reserve" title="[ reserve a node ]">
        <P>
          Infernet managed endpoints reserve one identified GPU operator
          with an exact set of models, by the hour. You get your own API
          keys, rate limits and usage reports, and pay only for hours that
          pass the uptime checks. Every <code>infernet reservation</code>{" "}
          command works through <code>c0mpute infernet</code>.
        </P>
        <Code>
{`c0mpute infernet reservation pricing        # price list by GPU class
c0mpute infernet reservation create --name N --operator O --models m1,m2
c0mpute infernet reservation key-issue <id> --label customer-a
c0mpute infernet reservation report <id>
c0mpute infernet reservation --help`}
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

      <Section h="bench" title="[ benchmark ]">
        <P>
          <code>c0mpute bench</code> measures the machine it runs on: recursive
          fibonacci, a dense matrix multiply and blake3 hashing, each at 1, 2,
          4, … N threads. The report is saved as <code>bench.json</code> in the
          data dir with the same <code>metadata</code> + <code>results</code>
          layout as fleetcode&apos;s runtime-benchmarks, and reduced to one
          score: 1000 means one reference core on every workload, so an
          8-vCPU cloud box lands near 7 000 and a 16-core desktop near 20 000.
        </P>
        <P>
          The worker reads the score at start and publishes it as{" "}
          <code>bench_score</code> in its capability ad, so buyers and the{" "}
          <Link href="/status">network status</Link> page see measured capacity.
          The <Link href="/benchmark">benchmark board</Link> shows CPU, cores,
          RAM and per-workload throughput for online nodes. Quick runs are for a
          sanity check and are never advertised.
        </P>
        <Code>
{`c0mpute bench                          # full run, saves + prints the table
c0mpute bench --quick                  # ~1s smoke run, not advertised
c0mpute bench --json                   # the report as JSON
c0mpute bench show                     # last saved report
c0mpute bench --threads 1,8,32         # your own thread ladder`}
        </Code>
      </Section>

      <Section h="agents" title="[ ai agents / webmcp ]">
        <P>
          c0mpute.com speaks{" "}
          <a href="https://webmachinelearning.github.io/webmcp/">WebMCP</a> — a
          browser AI agent visiting the site can call its tools directly instead
          of scraping the page. Tools register on{" "}
          <code>document.modelContext</code> and are a no-op in browsers that
          don&apos;t implement the (draft) spec.
        </P>
        <P>Tools exposed on every page:</P>
        <ul className="space-y-1 pl-5 text-sm leading-6">
          <li>
            <code>c0mpute_list_plugins</code> — the plugin catalogue with install
            commands (also at <Link href="/api/plugins">/api/plugins</Link>)
          </li>
          <li>
            <code>c0mpute_network_status</code> — live workers, jobs in flight,
            24h throughput, latency (also at <code>/api/status</code>)
          </li>
          <li>
            <code>c0mpute_latest_release</code> — CLI release manifest &amp;
            per-platform artifacts
          </li>
          <li>
            <code>c0mpute_install_command</code> — install command for the CLI or
            a plugin by id
          </li>
        </ul>
        <P>Any WebMCP-capable client can enumerate them:</P>
        <Code>
{`// in a page context on c0mpute.com
const tools = await document.modelContext.getTools();
const status = await document.modelContext.callTool("c0mpute_network_status", {});`}
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
