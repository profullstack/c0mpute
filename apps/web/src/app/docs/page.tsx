import Link from "next/link";
import { CodeBlock as Code } from "@/components/CodeBlock";
import { commandList } from "@/lib/chat/commands";

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

      <Section h="sign-in" title="[ sign in with CoinPay ]">
        <P>
          One c0mpute account works everywhere: the site and the installed app, chat, your node dashboard and fleet, the CLI and TUI,
          the API and MCP. Sign in at <Link href="/signin">/signin</Link> with <b>CoinPay</b> (OAuth 2.1 with PKCE; your CoinPay DID becomes your
          identity and payouts go to your CoinPay wallet) or a <b>passkey</b>. Passwords exist but are deprecated and will be removed.
          Add or remove methods, connect CoinPay and make API keys on <Link href="/account">/account</Link>.
        </P>
        <Code>
{`c0mpute login        # device sign-in: approve the code in your browser (any sign-in method)
c0mpute whoami       # the account, CoinPay DID and linked nodes
c0mpute logout       # forget this machine's key (~/.c0mpute/chat.json, mode 0600)
c0mpute chat login   # the same thing, kept for scripts that use it`}
        </Code>
        <P>
          API and MCP take <code>Authorization: Bearer c0k_...</code> (a key from <code>c0mpute login</code> or the account page). MCP clients
          (Claude, ChatGPT) can instead connect to <code>https://c0mpute.com/api/mcp</code> and sign in through OAuth; tools include{" "}
          <code>whoami</code>, <code>list_my_nodes</code>, <code>my_earnings</code>, <code>fleet_list</code>, <code>fleet_action</code>,{" "}
          <code>fleet_earnings</code>, <code>fleet_alerts_get</code>, <code>fleet_alerts_set</code> and <code>link_node</code>.
        </P>
        <Code>
{`curl -H "authorization: Bearer $C0MPUTE_API_KEY" https://c0mpute.com/api/v1/me?nodes=1
curl -H "authorization: Bearer $C0MPUTE_API_KEY" https://c0mpute.com/api/v1/account`}
        </Code>
      </Section>

      <Section h="link-node" title="[ link your node ]">
        <P>
          Linking proves you control a node: the server sends a one-shot challenge, the node signs it with its identity key (the key behind
          its peer id), and the node is verified to your account. Its earnings show on your account and it accepts fleet commands. Payouts
          need CoinPay connected.
        </P>
        <Code>
{`c0mpute login && c0mpute node link          # on the node
c0mpute node link --tags home,gpu --label den-3060

# Many boxes, no browser: an enrollment token (shown once, stored hashed, revocable)
c0mpute fleet token create --name rack-a --expires 7d --tags rack-a
curl -fsSL https://c0mpute.com/install.sh | sh -s -- --enroll c0e_...   # new boxes
c0mpute node link --token c0e_...                                       # installed boxes`}
        </Code>
      </Section>

      <Section h="fleet" title="[ manage a fleet ]">
        <P>
          The same fleet from every surface: <Link href="/dashboard/nodes">/dashboard/nodes</Link> (and the installed app), the TUI, the CLI,
          the API (<code>GET /api/v1/fleet</code>, <code>POST /api/v1/fleet/actions</code>) and MCP. Actions take one node, a selection, a tag
          or <code>--all</code>. pause, resume, update and restart are queued and signed by c0mpute.com; the node fetches them on its next poll
          (about a minute), checks the signature against the key it pinned when it linked, and reports back. Everything lands in the audit log.
        </P>
        <Code>
{`c0mpute fleet ls [--tag rack-a] [--json]
c0mpute fleet show <node>
c0mpute fleet pause|resume|update|restart <node...> | --tag rack-a | --all
c0mpute fleet tag <node...> --set vast,home  c0mpute fleet untag <node...> --set home
c0mpute fleet rename <node> "den 3060"       c0mpute fleet unlink <node...>
c0mpute fleet earnings [--json]              c0mpute fleet audit
c0mpute fleet token create|ls|revoke`}
        </Code>
        <P>
          Node settings are signed commands too. The node writes them to <code>fleet-settings.json</code> in its config dir (never to your
          config.toml), and reports its new state on the next poll, so the fleet view shows what each node really serves. Models and the
          house/uncensored switches take effect at once; hosting and storage restart the worker after its running jobs finish. A
          command-line <code>--no-storage</code> / <code>--no-hosting</code> on the node still wins. <code>default</code> hands a setting back to
          the node&apos;s own config.
        </P>
        <Code>
{`c0mpute fleet models <node...|--tag T|--all> --set qwen3:4b,llama3:8b   # serve only these (--clear: all)
c0mpute fleet house-models on|off|default <node...|--tag T|--all>
c0mpute fleet uncensored on|off|default <node...|--tag T|--all>
c0mpute fleet hosting on|off|default <node...|--tag T|--all>
c0mpute fleet storage on|off|default <node...|--tag T|--all>
# API: POST /api/v1/fleet/actions {"action":"hosting","tag":"rack-a","args":{"on":false}}
#      {"action":"models","nodes":["AbCd1234"],"args":{"models":["qwen3:4b"]}}`}
        </Code>
      </Section>

      <Section h="alerts" title="[ fleet alerts ]">
        <P>
          Email when a node goes offline (default after 15 minutes), the fleet&apos;s earnings drop (the last 24 hours against the trailing
          7-day daily average, default 50% down), an update fails, or a disk runs low (default under 10 GB or 5% free). One email when it
          starts and one when it is fixed; a node that flaps inside the cooldown (default 60 minutes) does not send more. The address is
          confirmed by a link first. Set it on <Link href="/account#alerts">your account</Link>, or:
        </P>
        <Code>
{`c0mpute fleet alerts set --email you@example.com --offline-minutes 15 --disk-min-gb 20 --off earnings
c0mpute fleet alerts get [--json]
c0mpute fleet alerts test
# API: GET|PUT /api/v1/fleet/alerts, POST {"test":true}   MCP: fleet_alerts_get, fleet_alerts_set`}
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
c0mpute infernet chat --model huihui_ai/qwen3.5-abliterated:4b "summarise this"
cat prompt.txt | c0mpute infernet chat --model huihui_ai/qwen3.5-abliterated:4b

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

      <Section h="api" title="[ chat API (OpenAI-compatible) ]">
        <P>
          Base URL <code>https://c0mpute.com/v1</code> with an API key
          (<code>c0k_...</code>) from the chat&apos;s keys tab:{" "}
          <code>POST /v1/chat/completions</code> and <code>GET /v1/models</code>.
          Any OpenAI SDK works. Honored: <code>model</code> (a model id, or{" "}
          <code>auto</code>, <code>auto:fast</code>, <code>auto:standard</code>,{" "}
          <code>auto:high</code>, <code>auto:max</code> for the router),{" "}
          <code>messages</code> (string content or text content parts; image
          parts get a 400, no vision model serves the API yet),{" "}
          <code>stream</code>, <code>temperature</code>,{" "}
          <code>max_tokens</code> (up to 8192), <code>response_format</code>{" "}
          (<code>json_object</code> / <code>json_schema</code>, as an
          instruction to the model) and the tool-calling fields below.
        </P>
        <P>
          <b>Tool calling.</b> <code>tools</code> (type <code>function</code>),{" "}
          <code>tool_choice</code> (<code>auto</code>, <code>none</code>,{" "}
          <code>required</code> or a named function),{" "}
          <code>parallel_tool_calls</code>, assistant messages with{" "}
          <code>tool_calls</code> and <code>role: &quot;tool&quot;</code>{" "}
          results with <code>tool_call_id</code>. Replies carry{" "}
          <code>choices[0].message.tool_calls</code> (ids, arguments as a JSON
          string) and <code>finish_reason: &quot;tool_calls&quot;</code>;
          streamed, they arrive as <code>delta.tool_calls</code> chunks, and{" "}
          <code>stream_options.include_usage</code> adds a usage chunk (token
          counts are estimated at about 4 characters a token). Works with the
          OpenAI SDKs, LangChain <code>bindTools</code>, the Vercel AI SDK,
          n8n&apos;s AI Agent, LiteLLM and opencode.
        </P>
        <P>
          Tool-capable models: <code>qwen3:4b</code>,{" "}
          <code>nemotron-3-nano:4b</code>, <code>qwen2.5-coder:7b</code>{" "}
          (tested on a node), and the same families at other sizes:{" "}
          <code>qwen3*</code>, <code>qwen3-coder</code>,{" "}
          <code>qwen2.5-coder:14b</code>, <code>qwen3.5</code> builds,{" "}
          <code>devstral</code>, <code>llama3.1+</code>,{" "}
          <code>mistral-nemo</code>. With <code>auto</code> a request with
          tools goes to the best tool-capable model online and moves to the
          next one if a node is busy. A request with tools for any other
          model is refused with a 400 naming the ones that work, so tools are
          never dropped silently. Every model uses the{" "}
          <code>&lt;tool_call&gt;</code> format the Qwen and Hermes families
          are trained on (what Ollama&apos;s own templates render), translated
          to and from OpenAI fields at c0mpute.com, so every node version
          serves tool requests. Each call is one ordinary chat job, billed the
          same as a chat turn; a reply with an unreadable tool call gets one
          repair retry.
        </P>
        <Code>
{`# 1. Offer a tool; the model asks for it
curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto",
       "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
       "tools": [{"type": "function", "function": {
         "name": "get_weather", "description": "Current weather for a city",
         "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]}'
# -> choices[0].finish_reason = "tool_calls"
#    choices[0].message.tool_calls = [{"id": "call_abc...", "type": "function",
#      "function": {"name": "get_weather", "arguments": "{\\"city\\":\\"Paris\\"}"}}]

# 2. Run the tool yourself, send the result back with the same id
curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto",
       "messages": [
         {"role": "user", "content": "What is the weather in Paris?"},
         {"role": "assistant", "content": null, "tool_calls": [{"id": "call_abc", "type": "function",
           "function": {"name": "get_weather", "arguments": "{\\"city\\":\\"Paris\\"}"}}]},
         {"role": "tool", "tool_call_id": "call_abc", "content": "{\\"temp_c\\": 17, \\"conditions\\": \\"light rain\\"}"}],
       "tools": [{"type": "function", "function": {
         "name": "get_weather", "description": "Current weather for a city",
         "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]}'
# -> choices[0].message.content = "It is 17 C with light rain in Paris."`}
        </Code>
      </Section>

      <Section h="house-lane" title="[ private house lane ]">
        <P>
          On the open network the node that wins a job decrypts and runs the prompt, so its operator could read it.
          For prompts that must not leave our hands (health records, finances), send{" "}
          <code>x-c0mpute-lane: house</code> (or <code>&quot;c0mpute&quot;: {"{"} &quot;lane&quot;: &quot;house&quot; {"}"}</code>{" "}
          in the body). The request then runs <b>only</b> on a node Profullstack operates, through an Infernet house-lane
          reservation; never on a third-party node, never on the free tier, never on a fallback.
        </P>
        <P>
          It is <b>paid only</b>: each answer is billed to your compute balance at the normal per-token price, and with no
          balance you get <code>402 balance_required</code> (top up at <Link href="/checkout">/checkout</Link>). If the
          house node is down you get <code>503 house_lane_unavailable</code> and nothing else runs it. Plain chat only (no
          tools, plugins or stored runs); <code>model</code> is ignored. Answers carry <code>x-c0mpute-lane: house</code>{" "}
          and <code>x_c0mpute.charged_usd</code>.
        </P>
        <P>
          <b>Who sees what:</b> the app that sent the prompt; c0mpute.com and Infernet in memory while relaying (not
          logged); Infernet holds it encrypted in its job queue only while the job runs, then deletes the prompt, the answer
          and the token stream (it keeps model, node, timestamps, token counts and the charge); and the house node that
          runs the model. No other operator. Apps can act for you with an OAuth key of scope <code>inference</code>{" "}
          (chat billed to your balance, and reading that balance; nothing else), revocable with{" "}
          <code>POST /api/oauth/revoke</code>.
        </P>
        <Code>
          {`curl https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "x-c0mpute-lane: house" \\
  -H "content-type: application/json" \\
  -d '{"messages":[{"role":"user","content":"Explain HbA1c 6.1% in plain words."}],"max_tokens":300}'`}
        </Code>
      </Section>

      <Section h="chat-commands" title="[ chat commands ]">
        <P>
          In <Link href="/chat">c0mpute.com/chat</Link> (and{" "}
          <code>c0mpute chat</code> / <code>c0mpute tui</code>) a line that
          starts with <code>/</code> is a command, never a message to the model;
          an unknown one answers with this list. <code>/video &lt;prompt&gt;</code>{" "}
          (also <code>/clip</code>, <code>/vid</code>) starts a clip through the
          API below and shows a card with its stage, place in line and ETA until
          the MP4 plays inline; it keeps polling across a page reload. In 18+
          mode it asks for an adult clip, which renders blurred until clicked.
        </P>
        <Code>{commandList()}</Code>
      </Section>

      <Section h="prompt-commands" title="[ prompt commands: /analyze and the finance pack ]">
        <P>
          Some commands are prompts, defined as markdown files in the repo&apos;s{" "}
          <code>commands/</code> directory (frontmatter <code>name</code>,{" "}
          <code>description</code>, <code>argument-hint</code>). They do go to
          the model: the server expands <code>/analyze &lt;question&gt;</code>{" "}
          into the command&apos;s instructions (added to the system turn) plus
          your question, then runs a normal turn with your chat plugins as
          tools. The expansion happens on the server, so it works the same in
          the web chat (type <code>/</code> for the list), <code>c0mpute chat
          &quot;/analyze ...&quot;</code>, the TUI, <code>POST /v1/chat/completions</code>{" "}
          and the MCP <code>chat</code> tool. The list is public at{" "}
          <code>GET /api/v1/chat/commands</code>. <code>/cashflow</code>,{" "}
          <code>/runway</code>, <code>/pnl</code>, <code>/spend</code>,{" "}
          <code>/forecast</code> and <code>/board</code> are the{" "}
          <Link href="/advisor/finance">Agentic Advisor: Finance</Link> pack:
          they read your Mercury, CoinPay or QuickBooks plugin, or a pasted CSV,
          validate the numbers and end with &quot;Analysis, not financial, tax
          or legal advice.&quot; To add a command, add one file to{" "}
          <code>commands/</code> and run{" "}
          <code>bun scripts/gen-chat-commands.ts</code>.
        </P>
      </Section>

      <Section h="videos" title="[ generate video (API) ]">
        <P>
          <code>POST /api/v1/videos</code> renders a clip on a c0mpute worker
          (Wan 2.1 1.3B on CPU by default, about 20 to 30 minutes once a worker
          is free; the network queues a clip for up to 2 hours). Authenticate
          with an API key from the chat&apos;s keys tab. It answers 202 with a
          status URL; send <code>Prefer: wait=N</code> (up to 7200 seconds) to
          get the MP4 in the same response when it finishes within N seconds,
          else the same 202. Billed per clip to your compute balance, or one
          free daily query.
        </P>
        <Code>
{`# An API key (c0k_...) from the chat's keys tab
export C0MPUTE_KEY=...

# Start a clip and hold the request until it is ready (up to 2 h)
curl -sS https://c0mpute.com/api/v1/videos \
  -H "Authorization: Bearer $C0MPUTE_KEY" \
  -H "Content-Type: application/json" \
  -H "Prefer: wait=7200" \
  -d '{"prompt": "YOUR PROMPT HERE"}' \
  -o clip.mp4 -w '%{http_code}\n'

# Or start it and poll
curl -sS https://c0mpute.com/api/v1/videos -H "Authorization: Bearer $C0MPUTE_KEY" \
  -H "Content-Type: application/json" -d '{"prompt": "YOUR PROMPT HERE"}'
# -> 202 {"id":"<id>","status":"pending","poll":"/api/v1/videos/<id>","video":"/api/v1/videos/<id>/video"}
curl -sS https://c0mpute.com/api/v1/videos/<id> -H "Authorization: Bearer $C0MPUTE_KEY"
curl -sS https://c0mpute.com/api/v1/videos/<id>/video -H "Authorization: Bearer $C0MPUTE_KEY" -o clip.mp4`}
        </Code>
        <P>
          Body: <code>prompt</code> (required), <code>model</code>{" "}
          (<code>wan2.1-t2v-1.3b</code> default, or <code>wan2.2-ti2v-5b</code>{" "}
          on GPU nodes), <code>negative_prompt</code>, <code>seed</code>,{" "}
          <code>adult</code>. Text to video only. If a 202 comes back before
          N seconds have passed, the hold was cut short by a proxy: keep polling
          the status URL. Every request passes the content policy (see{" "}
          <Link href="/terms">terms</Link>): sexual content involving minors or
          real, identifiable people is always refused, and{" "}
          <code>adult: true</code> needs an account with the 18+ attestation and
          a paid balance.
        </P>
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

      <Section h="autoscale" title="[ autoscale rented nodes ]">
        <P>
          Opt-in, for GPU nodes you <em>rent</em> by the hour (Vast.ai,
          SaladCloud, ...). <code>c0mpute autoscale</code> stops the rented
          instance when the network has no work for it and starts it again as
          soon as jobs for its models are waiting. A box you own saves nothing
          by stopping, so leave those out.
        </P>
        <P>
          Run it on any always-on machine you own (a home server, a fixed
          node), never on the rented box itself: it has to be awake to start
          the box again. Your cloud provider&apos;s API key stays on that
          machine, in a file or an environment variable. c0mpute.com never
          sees it: the CLI only reports the node&apos;s state to your{" "}
          <a href="/dashboard/nodes">dashboard</a> with your{" "}
          <code>C0MPUTE_API_KEY</code>.
        </P>
        <Code>
{`c0mpute autoscale init        # writes ~/.config/c0mpute/autoscale.toml (0600)
c0mpute autoscale once        # one decision per node, then exit
c0mpute autoscale run         # keep deciding (every minute)
c0mpute autoscale status      # each node: state, idle time, demand, saved`}
        </Code>
        <Code>
{`report = true            # send state to your c0mpute.com dashboard (needs C0MPUTE_API_KEY)

[[node]]
name = "vast-3060"
provider = "vast"         # vast | salad | thunder (thunder: status only, no native stop)
instance = "61205210"
node = "sdNT3kCN"         # board id or peer id
models = ["eriktonon/qwen3.8-uncensored:9b"]
api_key_file = "~/.config/c0mpute/vast.key"
idle_minutes = 20
min_run_minutes = 15
boot_timeout_minutes = 8
# busy_cmd = "~/bin/gpu-busy.sh"   # optional: exit 1 = idle; exit 0 or any failure = busy (never stop)
# pre_stop_cmd = "~/bin/stop-worker.sh"   # optional: run right before a stop
# dry_run = true          # watch only: log what it would do, never start/stop/rent

# [node.replace]          # vast only: rent a replacement if a start never comes up
# max_usd_per_hour = 0.12
# gpus = ["RTX 3060", "RTX 3070", "RTX 4060 Ti"]
# min_gpu_ram_gb = 12
# min_reliability = 0.98
# disk_gb = 60
# image = "nvidia/cuda:12.4.1-runtime-ubuntu22.04"
# onstart_file = "~/.config/c0mpute/vast-onstart.sh"
# identity_file = "~/.config/c0mpute/node-identity.key"   # keep the node id: sent as $C0MPUTE_IDENTITY_KEY_B64, your onstart writes it`}
        </Code>
        <P>
          <b>Stop</b> when all of these hold: the node has had no job for{" "}
          <code>idle_minutes</code>, nothing is in flight on it, the network
          has no waiting or recent demand for its models,{" "}
          <code>busy_cmd</code> (if set) says idle, and it has run at least{" "}
          <code>min_run_minutes</code> since it was started.{" "}
          <b>Start</b> at once when the network has offers for its models
          (waiting, or in the last 2 minutes) or recent demand nobody served.
          The demand comes from the status aggregators (
          <code>GET /api/v1/autoscale/signal?node=&lt;id&gt;&amp;models=a,b</code>
          ), which watch every job offer on the network.
        </P>
        <P>
          Providers: on Vast.ai a stopped instance still bills its disk, and
          its GPU can go to another renter while it is stopped, so a start can
          fail; set <code>[node.replace]</code> and the CLI rents a matching
          offer under your price cap after <code>boot_timeout_minutes</code>{" "}
          and destroys the old instance only once the new one serves.
          SaladCloud container groups are stopped and started. Thunder Compute
          has no native stop (only snapshot and delete), so it is status only
          for now.
        </P>
        <Code>
{`# ~/.config/systemd/user/c0mpute-autoscale.service
[Unit]
Description=c0mpute autoscale: stop idle rented nodes, start them on demand
After=network-online.target

[Service]
EnvironmentFile=-%h/.config/c0mpute/autoscale.env   # C0MPUTE_API_KEY=... (0600)
ExecStart=%h/.c0mpute/bin/c0mpute autoscale run
Restart=always
RestartSec=30

[Install]
WantedBy=default.target

# then: systemctl --user enable --now c0mpute-autoscale
#       loginctl enable-linger $USER     # keep it running after you log out`}
        </Code>
        <P>
          The <a href="/dashboard/nodes">dashboard</a> shows each autoscaled
          node&apos;s state (running, stopped, starting, ...), its last
          change and why, and what stopping it has saved. Full reference:{" "}
          <a href="https://github.com/profullstack/c0mpute/blob/master/docs/autoscale.md">docs/autoscale.md</a>.
        </P>
      </Section>

      <Section h="crawl" title="[ distributed scraping ]">
        <P>
          <code>crawl.fetch</code> fetches one public page from a worker&apos;s
          own connection: a static <code>GET</code> (no JavaScript), robots.txt
          honoured, public addresses only (re-checked on every redirect), 1 MiB
          per page, 15 s default and 30 s maximum. Every worker takes crawl
          jobs by default, up to 8 at once. Send one job per URL through the
          jobs bridge and each URL gets its own auction, so a list of pages
          spreads over the network&apos;s workers and their IPs.
        </P>
        <Code>
{`# one page, no bridge
c0mpute crawl fetch https://example.com/ --max-price 0.01
c0mpute crawl fetch https://mysite.example/ --ignore-robots   # only a site you own

# many pages: start a buyer bridge (runs up to 32 jobs at once)
export C0MPUTE_JOBS_TOKEN=$(openssl rand -hex 24)
c0mpute jobs serve --listen 127.0.0.1:8788 --max-price 0.01

# one request per URL
curl -s http://127.0.0.1:8788/v1/jobs \\
  -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \\
  -d '{"workload": "crawl.fetch",
       "spec": {"v": 1, "url": "https://example.com/",
                "headers": {"accept-language": "en"}, "timeout_ms": 20000},
       "max_price": 0.005, "timeout_ms": 60000}'
# {"job_id":"…","worker":"12D3KooW…","price":0.0,"output_hash":"…",
#  "result":{"v":1,"ok":true,"duration_ms":412,
#            "output":{"url":"https://example.com/","status":200,"headers":{…},"body":"<!doctype html>…"}}}

# fan a list out, 8 in flight
grep -v '^#' urls.txt | xargs -P 8 -I{} curl -s http://127.0.0.1:8788/v1/jobs \\
  -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \\
  -d '{"workload":"crawl.fetch","spec":{"v":1,"url":"{}"}}' \\
  | jq -c '{url: .result.output.url, worker, status: .result.output.status}'`}
        </Code>
        <P>
          Request: <code>workload</code>, <code>spec</code> (<code>v</code>,{" "}
          <code>url</code>, optional <code>headers</code>,{" "}
          <code>timeout_ms</code>, <code>ignore_robots</code>), optional{" "}
          <code>max_price</code> (may only lower the bridge&apos;s{" "}
          <code>--max-price</code>), <code>allow_peers</code> and{" "}
          <code>timeout_ms</code> for the whole job (1 s to 300 s). A refused
          or failed fetch answers 200 with <code>&quot;ok&quot;: false</code>{" "}
          and an <code>error</code>. Bridge errors are{" "}
          <code>{`{"error": "<code>", "detail": "…"}`}</code>: 401{" "}
          <code>unauthorized</code>, 503 <code>no_bids</code>, 502{" "}
          <code>worker_failed</code>, 504 <code>timeout</code>; a retry is a
          fresh auction. The worker sees the URL and headers, so keep secrets
          out or pin trusted workers with <code>allow_peers</code>. Runnable
          script:{" "}
          <a href="https://github.com/profullstack/c0mpute/tree/master/examples/distributed-scraper">
            examples/distributed-scraper
          </a>
          .
        </P>
      </Section>

      <Section h="jobs" title="[ track jobs ]">
        <Code>
{`c0mpute job status <job-id>          # one-shot status
c0mpute job logs <job-id> --follow   # tail logs
c0mpute job cancel <job-id>          # cancel queued/running
c0mpute tui                          # terminal app: chat, jobs, node, network`}
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
{`# Install a first-party plugin by id (from the signed release archive)
c0mpute plugin install my-plugin

# Install a third-party plugin from its install.sh (not verified by c0mpute)
c0mpute plugin install --unverified https://example.com/my-plugin/install.sh

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
