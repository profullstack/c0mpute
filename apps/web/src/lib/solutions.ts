/**
 * Solutions: one entry per c0mpute feature in the gallery's "built with"
 * vocabulary (BUILT_WITH in lib/gallery.ts). The Solutions menu, /solutions and
 * /solutions/<slug> all read this list, and solutions.test.ts fails if it and
 * BUILT_WITH drift apart.
 *
 * Keep every claim checkable: a command that exists, a route that answers, a
 * page that is live. No prices and no numbers that change without a deploy.
 * Commands here are copied from the docs, the CLI's own help and the route
 * handlers; when one of those changes, change it here too.
 */

export type SolutionLink = { href: string; label: string };

export type UseCase = {
  title: string;
  /** Who does this and why, in a sentence or two. */
  text: string;
  /** A concrete example, shown in a code block. */
  example: string;
};

export type Solution = {
  /** URL segment, lowercase kebab-case. Never changes once published. */
  slug: string;
  /** Exactly the BUILT_WITH label, so gallery entries match on it. */
  label: string;
  /** Short line for the menu. */
  desc: string;
  /** One or two sentences: what it is. */
  summary: string;
  /** Everything needed before the first example runs. */
  install: string;
  /** The smallest thing that works. */
  example: string;
  /** What comes back, in a sentence. */
  result: string;
  useCases: UseCase[];
  /** Where to go next. Site-relative or absolute. */
  links: SolutionLink[];
};

const REPO = "https://github.com/profullstack/c0mpute/blob/master";

const INSTALL = "curl -fsSL https://c0mpute.com/install.sh | sh";

const API_KEY = `# an API key (c0k_...) from the keys tab at https://c0mpute.com/chat
export C0MPUTE_KEY=c0k_...`;

const CHAT_ONLY = `# nothing to install: sign in at
https://c0mpute.com/chat`;

export const SOLUTIONS: Solution[] = [
  {
    slug: "hosting",
    label: "c0mpute hosting",
    desc: "sites on the network, self-healing copies",
    summary:
      "Publish a folder as a website held by nodes on the network. Copies are kept on several machines and repaired when one goes away. Every site gets a gateway URL and can take a custom domain.",
    install: INSTALL,
    example: `c0mpute hosting publish ./public --name my-site`,
    result:
      "The site is live at https://gateway.c0mpute.com/s/<site_id>/ once its copies are placed. Publishing the same name again is a new version of the same site.",
    useCases: [
      {
        title: "A blog or docs site on your own domain",
        text: "Build with any static generator, publish the output folder, and point a domain at it. Publish again after each build.",
        example: `npm run build
c0mpute hosting publish ./dist --name my-blog --domain blog.example.org`,
      },
      {
        title: "Publish what a chat just made",
        text: "Ask the chat to write a page, then publish the files from that conversation without leaving it.",
        example: `# in c0mpute.com/chat
/push hosting my-landing-page
/push hosting ls
/push hosting rm my-landing-page`,
      },
      {
        title: "A private site",
        text: "Seal the whole site on your machine before it leaves. Nodes and gateways hold only ciphertext; the browser decrypts it with a key in the link that is never sent to the gateway.",
        example: `c0mpute hosting publish ./dir --name team-notes --private
# -> https://gateway.c0mpute.com/p/<id>/#k=<key>`,
      },
    ],
    links: [
      { href: "/apps", label: "apps & hosting" },
      { href: "/dashboard/disk", label: "my hosting" },
      { href: `${REPO}/docs/hosting.md`, label: "hosting docs" },
    ],
  },
  {
    slug: "chat-api",
    label: "c0mpute chat API",
    desc: "OpenAI-compatible, with tool calling",
    summary:
      "An OpenAI-compatible chat completions endpoint answered by open models on network nodes. Streaming and tool calling work, so existing OpenAI clients need only a base URL and a key.",
    install: API_KEY,
    example: `curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto", "messages": [{"role": "user", "content": "hello"}]}'`,
    result: "A standard chat completion object. List the models online with GET https://c0mpute.com/v1/models.",
    useCases: [
      {
        title: "Point an existing OpenAI app at it",
        text: "Any OpenAI SDK works: change the base URL and the key, keep the rest of the code.",
        example: `from openai import OpenAI

client = OpenAI(base_url="https://c0mpute.com/v1", api_key="c0k_...")
reply = client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "Summarise this ticket in one line: ..."}],
)
print(reply.choices[0].message.content)`,
      },
      {
        title: "An agent that calls your tools",
        text: "Offer functions with the standard tools field. The model asks for one, you run it and send the result back with the same id.",
        example: `curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto",
       "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
       "tools": [{"type": "function", "function": {
         "name": "get_weather", "description": "Current weather for a city",
         "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]}'
# -> choices[0].finish_reason = "tool_calls"`,
      },
      {
        title: "Structured output for a pipeline",
        text: "Ask for JSON and parse the reply in a script: classify tickets, extract fields, tag content.",
        example: `curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto", "response_format": {"type": "json_object"},
       "messages": [{"role": "user",
         "content": "Return JSON with keys sentiment and topic for: the update broke my login"}]}'`,
      },
    ],
    links: [
      { href: "/docs#api", label: "chat API docs" },
      { href: "/chat", label: "get an API key" },
      { href: "/agents", label: "agents & MCP" },
    ],
  },
  {
    slug: "infernet-chat",
    label: "Infernet chat",
    desc: "chat with open models, web or CLI",
    summary:
      "Chat with open models served by Infernet provider nodes, in the browser or from the terminal. The same chat runs the slash commands for images, music, video and publishing.",
    install: `# browser: nothing to install
https://c0mpute.com/chat

# terminal
${INSTALL}
c0mpute chat login`,
    example: `c0mpute chat "explain rendezvous hashing in two sentences"`,
    result: "The answer streams back from a node on the network. Conversations are the same in the browser and the terminal.",
    useCases: [
      {
        title: "A terminal assistant",
        text: "Ask from the shell you are already in, and pipe the answer where you need it.",
        example: `c0mpute chat "write a jq filter that keeps objects where status is failed"`,
      },
      {
        title: "Pick the model yourself",
        text: "See which models are online and how many nodes serve each, then ask one by name.",
        example: `c0mpute pool                 # live nodes per model
c0mpute pool qwen
c0mpute infernet chat --model huihui_ai/qwen3.5-abliterated:4b "summarise this"`,
      },
      {
        title: "One chat for text, pictures, music and publishing",
        text: "The slash commands run the other solutions from the same conversation.",
        example: `# in c0mpute.com/chat
/image a lighthouse in a storm, oil painting
/music lo-fi hip hop for a rainy night
/push hosting my-site`,
      },
    ],
    links: [
      { href: "/chat", label: "open chat" },
      { href: "/docs#chat-commands", label: "chat commands" },
      { href: "https://infernetprotocol.com", label: "infernetprotocol.com" },
    ],
  },
  {
    slug: "jobs",
    label: "c0mpute jobs",
    desc: "batch work, sealed-bid under your cap",
    summary:
      "Run batch work on other people's machines. Every job is a sealed-bid auction under the price cap you set, the lowest bid wins, and the result comes back with a receipt.",
    install: INSTALL,
    example: `c0mpute transcode submit input.mov --preset hls --max-price 1.25`,
    result: "Nodes bid under your cap, one runs the job, and you get the output and a receipt. Follow it with the commands under \"track jobs\" in the docs.",
    useCases: [
      {
        title: "Transcode video without a render box",
        text: "Send a file, name a preset and a cap. Good for a backlog of uploads or a one-off 4K encode.",
        example: `c0mpute transcode preset list
c0mpute transcode submit input.mp4 --preset video-4k --max-price 5.00`,
      },
      {
        title: "A distributed scraper",
        text: "Fetch many public pages from many machines: one job per URL, each its own auction. robots.txt is honoured and private addresses are refused.",
        example: `export C0MPUTE_JOBS_TOKEN=$(openssl rand -hex 24)
c0mpute jobs serve --max-price 0.01 &
grep -v '^#' urls.txt | xargs -P 8 -I{} curl -s http://127.0.0.1:8788/v1/jobs \\
  -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \\
  -d '{"workload":"crawl.fetch","spec":{"v":1,"url":"{}"}}'`,
      },
      {
        title: "Workflow steps from your own automation",
        text: "Run a transform or a webhook step on the network from any tool that can make an HTTP call. Step payloads are visible to the worker that wins, so keep secrets out of them.",
        example: `curl -s http://127.0.0.1:8788/v1/jobs -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \\
  -d '{"workload":"workflow.step","spec":{"v":1,"kind":"transform",
       "config":{"expression":"items[].name"},"input":{"items":[{"name":"a"}]}}}'`,
      },
    ],
    links: [
      { href: "/market", label: "market" },
      { href: "/docs#transcode", label: "submit a job" },
      { href: "/docs#crawl", label: "distributed scraping" },
      { href: "/docs#jobs", label: "track jobs" },
    ],
  },
  {
    slug: "music",
    label: "c0mpute music",
    desc: "songs and albums from a prompt",
    summary:
      "Songs rendered on network workers with ACE-Step 1.5, with or without vocals, and whole albums: a band, named songs with lyrics, cover art and a zip.",
    install: CHAT_ONLY,
    example: `/music lo-fi hip hop for a rainy night`,
    result: "An instrumental track, playable in the chat, in a few minutes.",
    useCases: [
      {
        title: "A music bed for a video or podcast",
        text: "Describe the style and get an instrumental you can put under narration.",
        example: `/music warm acoustic guitar, slow, hopeful, no drums`,
      },
      {
        title: "A song with your words",
        text: "Add a line that says lyrics: and then the words, and the track is sung.",
        example: `/music synthpop
lyrics:
[verse]
neon on the water, static in the rain
[chorus]
we were never offline`,
      },
      {
        title: "A whole album from one idea",
        text: "One prompt becomes a band, eight named songs with lyrics, cover art and a zip to download.",
        example: `/album dark synthwave about a drowned city`,
      },
    ],
    links: [
      { href: "/chat", label: "open chat" },
      { href: "/docs#chat-commands", label: "chat commands" },
    ],
  },
  {
    slug: "speech",
    label: "c0mpute speech",
    desc: "text to speech on network nodes",
    summary:
      "Text to speech rendered by nodes on the network, for narration, podcasts and voice calls. A small local bridge takes your text over HTTP and buys the audio from the network.",
    install: `${INSTALL}
export C0MPUTE_SPEECH_TOKEN=$(openssl rand -hex 24)
c0mpute speech serve --max-price-usd 0.01 &     # listens on 127.0.0.1:8787`,
    example: `curl -s http://127.0.0.1:8787/v1/tts \\
  -H "Authorization: Bearer $C0MPUTE_SPEECH_TOKEN" -H "Content-Type: application/json" \\
  -d '{"text": "Hello from the network.", "engine": "kokoro", "voice": "af_sarah"}' \\
  -o hello.wav`,
    result: "A WAV file. GET /v1/voices on the same bridge lists every engine and voice.",
    useCases: [
      {
        title: "Narrate an article or a script",
        text: "Send paragraphs one at a time and join the files: a blog post becomes an audio version.",
        example: `curl -s http://127.0.0.1:8787/v1/voices | head -40      # choose a voice
curl -s http://127.0.0.1:8787/v1/tts \\
  -H "Authorization: Bearer $C0MPUTE_SPEECH_TOKEN" -H "Content-Type: application/json" \\
  -d '{"text": "Chapter one. It began with a deploy.", "engine": "kokoro", "voice": "af_sarah", "speed": 0.95}' \\
  -o chapter-1.wav`,
      },
      {
        title: "Transcribe a recording",
        text: "The same bridge serves speech to text at /v1/stt: post the audio as the request body, for meeting notes or captions.",
        example: `curl -s "http://127.0.0.1:8787/v1/stt?language=en" \\
  -H "Authorization: Bearer $C0MPUTE_SPEECH_TOKEN" -H "Content-Type: audio/wav" \\
  --data-binary @meeting.wav`,
      },
      {
        title: "Serve voices from your own node",
        text: "A worker installs CPU voices on first start. Check or change that on a machine you run.",
        example: `c0mpute tts voices             # supported voices
c0mpute tts setup              # install the CPU engines now
c0mpute tts auto off           # leave speech to other nodes`,
      },
    ],
    links: [
      { href: "/docs", label: "CLI docs" },
      { href: "/providers", label: "run a node" },
    ],
  },
  {
    slug: "images",
    label: "c0mpute images",
    desc: "pictures from a prompt, on CPU workers",
    summary:
      "Pictures drawn on network CPU workers with Z-Image-Turbo. Ask in chat and the image arrives in about two minutes. No GPU of your own is involved.",
    install: CHAT_ONLY,
    example: `/image a lighthouse in a storm, oil painting`,
    result: "A PNG in the conversation, in about two minutes.",
    useCases: [
      {
        title: "A hero image for a post or landing page",
        text: "Describe the subject and the style. Ask again with a changed prompt until it fits.",
        example: `/image isometric illustration of a small server rack on a desk, soft pastel colours`,
      },
      {
        title: "Cover art",
        text: "Art for a podcast episode, a playlist or a release.",
        example: `/image album cover, a drowned city at night seen from above, synthwave palette`,
      },
      {
        title: "Publish it straight away",
        text: "Draw the image, have the chat write a page around it, and publish both.",
        example: `/image product shot of a ceramic mug on a wooden table, morning light
# then: "write a one-page site for this mug using that image"
/push hosting mug-shop`,
      },
    ],
    links: [
      { href: "/chat", label: "open chat" },
      { href: "/docs#chat-commands", label: "chat commands" },
    ],
  },
  {
    slug: "video",
    label: "c0mpute video",
    desc: "short clips and promo videos",
    summary:
      "Short clips rendered on network workers with the Wan models, from chat or over the API, and promo videos of a website with narration and music. A /video is 30 seconds by default (5 to 60), Wan 2.2 5B shots planned around your subjects and cut together on GPU workers.",
    install: API_KEY,
    example: `curl -sS https://c0mpute.com/api/v1/videos \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -H "Prefer: wait=7200" \\
  -d '{"prompt": "a paper boat crossing a puddle"}' \\
  -o clip.mp4 -w '%{http_code}\\n'`,
    result: "An MP4 when it finishes inside the wait, otherwise a 202 with a status URL to poll.",
    useCases: [
      {
        title: "A clip from chat",
        text: "No key and no code: ask in the chat and come back for it.",
        example: `# in c0mpute.com/chat
/video a paper boat crossing a puddle`,
      },
      {
        title: "A promo video of your site",
        text: "Give it a URL and a look (pixar, anime, claymation or flat) and get a promo video with narration and music. Add vertical for phones.",
        example: `/promo https://example.org pixar
/promo https://example.org anime vertical`,
      },
      {
        title: "Start a clip from a script and poll",
        text: "Queue it from a job or a build step and fetch the file when it is done.",
        example: `curl -sS https://c0mpute.com/api/v1/videos -H "Authorization: Bearer $C0MPUTE_KEY" \\
  -H "Content-Type: application/json" -d '{"prompt": "a paper boat crossing a puddle"}'
# -> 202 {"id":"<id>","status":"pending","poll":"/api/v1/videos/<id>","video":"/api/v1/videos/<id>/video"}
curl -sS https://c0mpute.com/api/v1/videos/<id> -H "Authorization: Bearer $C0MPUTE_KEY"
curl -sS https://c0mpute.com/api/v1/videos/<id>/video -H "Authorization: Bearer $C0MPUTE_KEY" -o clip.mp4`,
      },
    ],
    links: [
      { href: "/docs#videos", label: "video API docs" },
      { href: "/chat", label: "open chat" },
    ],
  },
  {
    slug: "fine-tune",
    label: "fine-tune",
    desc: "train a model on your own writing",
    summary:
      "Give c0mpute a folder of writing and a small open model, and a worker on the network fine-tunes it while you watch the loss fall. Then chat with the result.",
    install: INSTALL,
    example: `cat > job.json <<'EOF'
{ "name": "my-agent", "kind": "lora",
  "base_model": "HuggingFaceTB/SmolLM2-135M-Instruct",
  "system_prompt": "You are a concise, practical business strategist.",
  "hyperparams": { "max_steps": 200, "batch_size": 2, "seq_len": 256 } }
EOF

# corpus.jsonl: one {"title": "...", "text": "..."} per line
c0mpute infernet text train --job job.json --dataset corpus.jsonl --output ./adapter --yes`,
    result: "A LoRA adapter in ./adapter, with a live progress bar while it trains.",
    useCases: [
      {
        title: "An assistant that sounds like your company",
        text: "Train on your blog, docs or support answers, then ask it questions in your own voice.",
        example: `c0mpute infernet text chat --adapter ./adapter "How should I price my SaaS?"`,
      },
      {
        title: "Training you can check",
        text: "Run the same job on several workers and compare, with a cap on what each may charge.",
        example: `c0mpute train lora --base Qwen/Qwen2.5-0.5B-Instruct --dataset data.jsonl \\
  --replicas 3 --max-price 1 --yes`,
      },
      {
        title: "Earn by training for others",
        text: "Set a machine up to take training and chat jobs from the network.",
        example: `${INSTALL}
npm i -g @infernetprotocol/cli
infernet model runtime setup --peft --yes
c0mpute infernet text serve`,
      },
    ],
    links: [
      { href: "/train", label: "train an AI agent" },
      { href: "/gallery?category=models", label: "fine-tuned models in the gallery" },
    ],
  },
  {
    slug: "model-router",
    label: "model router",
    desc: 'the model behind model: "auto"',
    summary:
      'Send model "auto" and the router picks the model and execution pattern for each request, checks the answer and falls back when a model fails. A free dry run shows the decision without running anything.',
    install: `# to see a routing decision: nothing, the dry run is public
# to run requests through it:
${API_KEY}`,
    example: `curl -sS https://c0mpute.com/api/v1/route -H "Content-Type: application/json" \\
  -d '{"model": "auto", "messages": [{"role": "user", "content": "Refactor this function to be iterative: ..."}]}'`,
    result:
      "How the request was classified, the plan the router would run (pattern, models, fallbacks, expected quality, cost and latency, and why), and the next-best alternatives.",
    useCases: [
      {
        title: "Stop choosing a model per request",
        text: "Use one model name everywhere and let easy questions go to small models and hard ones to larger ones.",
        example: `curl -sS https://c0mpute.com/v1/chat/completions \\
  -H "Authorization: Bearer $C0MPUTE_KEY" -H "Content-Type: application/json" \\
  -d '{"model": "auto", "messages": [{"role": "user", "content": "hello"}]}'`,
      },
      {
        title: "Cap cost and latency",
        text: "Tell the router what a request may cost and how long it may take, and it plans inside that.",
        example: `curl -sS https://c0mpute.com/api/v1/route -H "Content-Type: application/json" \\
  -d '{"model": "auto", "router": {"max_cost_usd": 0.01, "max_latency_ms": 20000},
       "messages": [{"role": "user", "content": "Draft a polite refund reply."}]}'`,
      },
      {
        title: "Explain a decision before you ship it",
        text: "Dry-run the prompts your product sends and read why each one would go where it goes.",
        example: `for p in "2+2?" "Prove that sqrt(2) is irrational." ; do
  curl -sS https://c0mpute.com/api/v1/route -H "Content-Type: application/json" \\
    -d "{\\"messages\\": [{\\"role\\": \\"user\\", \\"content\\": \\"$p\\"}]}"
done`,
      },
    ],
    links: [
      { href: "/docs#api", label: "chat API docs" },
      { href: "/solutions/chat-api", label: "c0mpute chat API" },
    ],
  },
  {
    slug: "apps",
    label: "c0mpute apps",
    desc: "backends and containers on the network",
    summary:
      "Deploy a Dockerfile as a running backend: an API, an MCP server, a bot, a worker with a SQLite database. The network keeps replicas on separate apps nodes (three by default, when that many are online), runs one, snapshots its state and promotes a standby when the primary goes away.",
    install: `${INSTALL}
c0mpute storage key init --backup-to id.backup   # once: the identity that owns your apps`,
    example: `c0mpute apps deploy ./api --name api --port 8080 --state /data`,
    result: "The app is built, published and started, and served by gateways at /a/<app_id>/. c0mpute apps status api shows its holders and health.",
    useCases: [
      {
        title: "An API with a database",
        text: "Any container that serves HTTP on one port. Name a state directory and it is snapshotted and carried to the next holder.",
        example: `c0mpute apps deploy ./api --name api --port 8080 --state /data
c0mpute apps secrets set api DATABASE_KEY=...
c0mpute apps logs api`,
      },
      {
        title: "Deploy with git push",
        text: "Add a c0mpute remote and push. A repo with an [app] section in c0mpute.toml deploys on push.",
        example: `c0mpute git install
git remote add c0 c0mpute://api
git push c0 main`,
      },
      {
        title: "Manage it from any of your machines",
        text: "An app belongs to your account key, not to the machine that deployed it. The same name is the same app from a laptop, a server or CI.",
        example: `c0mpute apps deploy ./api --name api      # from the laptop
c0mpute apps logs api                     # from the server: same app
c0mpute apps destroy api --yes            # from anywhere`,
      },
    ],
    links: [
      { href: "/apps", label: "apps & hosting" },
      { href: "/dashboard/disk", label: "my hosting" },
      { href: `${REPO}/docs/apps.md`, label: "apps docs" },
    ],
  },
];

export const solutionBySlug = (slug: string) => SOLUTIONS.find((s) => s.slug === slug);

/** The solution page for a gallery "built with" label, if there is one. */
export const solutionForLabel = (label: string) => SOLUTIONS.find((s) => s.label === label);

export const solutionPath = (s: Solution) => `/solutions/${s.slug}`;
