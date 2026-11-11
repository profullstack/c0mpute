// Renderer: a small DOM UI over the main process (window.c0).

type Any = any; // eslint-disable-line @typescript-eslint/no-explicit-any
declare global {
  interface Window {
    c0: { invoke(ch: string, ...a: unknown[]): Promise<Any>; on(ev: string, cb: (p: Any) => void): () => void };
  }
}

const c0 = window.c0;
const $ = (s: string) => document.querySelector(s) as HTMLElement;

type Child = Node | string | number | null | undefined | false | Child[];
function h(tag: string, attrs: Record<string, Any> = {}, ...kids: Child[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "value") (el as HTMLInputElement).value = v;
    else if (k === "checked") (el as HTMLInputElement).checked = !!v;
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  const add = (c: Child) => {
    if (c === null || c === undefined || c === false) return;
    if (Array.isArray(c)) c.forEach(add);
    else el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  };
  kids.forEach(add);
  return el;
}

function toast(msg: string, err = false) {
  const t = h("div", { class: `toast${err ? " err" : ""}` }, msg);
  document.body.append(t);
  setTimeout(() => t.remove(), err ? 9000 : 4000);
}

async function call<T = Any>(ch: string, ...a: unknown[]): Promise<T | undefined> {
  try {
    return await c0.invoke(ch, ...a);
  } catch (e) {
    toast(e instanceof Error ? e.message.replace(/^Error invoking remote method '[^']+': (Error: )?/, "") : String(e), true);
    return undefined;
  }
}

function btn(label: string, onclick: () => unknown, cls = "b") {
  const b = h("button", { class: cls }, label) as HTMLButtonElement;
  b.addEventListener("click", async () => {
    b.disabled = true;
    try {
      await onclick();
    } finally {
      b.disabled = false;
    }
  });
  return b;
}

function bar(frac: number | undefined) {
  const i = h("i");
  i.style.width = `${Math.round(Math.max(0, Math.min(1, frac ?? 0)) * 100)}%`;
  return h("div", { class: "bar" }, i);
}

function modal(content: HTMLElement) {
  const m = $("#modal");
  m.replaceChildren(h("div", { class: "dialog" }, content));
  m.hidden = false;
  setChatVisible(false);
  return () => {
    m.hidden = true;
    m.replaceChildren();
    if (tab === "chat") layoutChat();
  };
}

const ago = (t: number) => {
  const s = Math.round((Date.now() - t) / 1000);
  return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.round(s / 60)}m ago` : `${Math.round(s / 3600)}h ago`;
};

// ── tabs ───────────────────────────────────────────────────────────────

let tab = "chat";
let cleanup: (() => void)[] = [];
const main = $("#main");

function setChatVisible(v: boolean) {
  call("chat:bounds", { x: 0, y: 0, width: 0, height: 0, ...(v ? chatRect() : {}), visible: v });
}
function chatRect() {
  const box = document.getElementById("chatbox");
  if (!box) return { x: 0, y: 0, width: 0, height: 0 };
  const r = box.getBoundingClientRect();
  return { x: r.left, y: r.top, width: r.width, height: r.height };
}
function layoutChat() {
  if (tab === "chat" && $("#modal").hidden) c0.invoke("chat:bounds", { ...chatRect(), visible: true });
}

async function show(name: string) {
  cleanup.forEach((f) => f());
  cleanup = [];
  tab = name;
  document.querySelectorAll("#nav button").forEach((b) => b.classList.toggle("on", (b as HTMLElement).dataset.tab === name));
  main.classList.toggle("flush", name === "chat");
  main.replaceChildren();
  if (name !== "chat") setChatVisible(false);
  await views[name]?.();
}

document.querySelectorAll("#nav button").forEach((b) => b.addEventListener("click", () => show((b as HTMLElement).dataset.tab!)));

// ── chat ───────────────────────────────────────────────────────────────

function chatView() {
  main.append(
    h("div", { class: "chatbar" }, btn("Back", () => call("chat:back")), btn("Reload", () => call("chat:reload")), h("span", { class: "dim" }, "c0mpute.com/chat, signed in with CoinPay: /image, /music, /album, /promo, /connect hosting, /push hosting <name>")),
    h("div", { id: "chatbox" }),
  );
  const ro = new ResizeObserver(() => layoutChat());
  ro.observe(main);
  window.addEventListener("resize", layoutChat);
  cleanup.push(() => {
    ro.disconnect();
    window.removeEventListener("resize", layoutChat);
  });
  requestAnimationFrame(layoutChat);
}

// ── agent ──────────────────────────────────────────────────────────────

const transcript: HTMLElement = h("div", { class: "transcript" });
let agentStatus: HTMLElement = h("span", { class: "dim" });
const toolNodes = new Map<string, HTMLElement>();

function renderEvent(ev: Any) {
  switch (ev.type) {
    case "user":
      transcript.append(h("div", { class: "msg user" }, ev.text));
      break;
    case "assistant":
      transcript.append(h("div", { class: "msg" }, ev.text, ev.model ? h("div", { class: "dim mono" }, ev.model) : null));
      break;
    case "tool_call": {
      const d = h("details", { class: "msg tool", open: true }, h("summary", {}, `→ ${ev.name} `, h("span", { class: "tag" }, ev.source)), h("pre", {}, JSON.stringify(ev.arguments, null, 2)));
      toolNodes.set(ev.id, d);
      transcript.append(d);
      break;
    }
    case "tool_result": {
      const d = toolNodes.get(ev.id);
      const out = h("pre", { class: ev.error ? "bad" : "" }, `${ev.error ? "✗ " : "← "}${ev.output}`);
      if (d) {
        d.append(out);
        (d as HTMLDetailsElement).open = !!ev.error;
      } else transcript.append(h("div", { class: "msg tool" }, out));
      break;
    }
    case "status":
      agentStatus.textContent = ev.text;
      break;
    case "error":
      transcript.append(h("div", { class: "msg err" }, ev.text));
      break;
    case "done":
      agentStatus.textContent = "";
      break;
  }
  if (tab === "agent") main.scrollTop = main.scrollHeight;
}
c0.on("agent:event", renderEvent);

c0.on("agent:confirm", (req: Any) => {
  const close = modal(
    h(
      "div",
      {},
      h("h1", {}, req.source === "shell" ? "Run this command?" : "Run this tool?"),
      h("p", {}, h("b", {}, req.name), " ", h("span", { class: "tag" }, req.source)),
      h("pre", { class: "log" }, req.source === "shell" ? String(req.arguments.command ?? "") : JSON.stringify(req.arguments, null, 2)),
      h(
        "div",
        { class: "row", style: "margin-top:12px" },
        btn("Run once", () => reply("once"), "b primary"),
        req.canAlways ? btn(`Always allow ${req.source} (this session)`, () => reply("always")) : null,
        btn("Deny", () => reply("deny"), "b danger"),
      ),
    ),
  );
  function reply(d: string) {
    call("agent:confirmReply", req.id, d);
    close();
  }
});

async function agentView() {
  const info = await call("app:info");
  const models: string[] = (await call("agent:models")) ?? [];
  const tools: Any[] = (await call("agent:tools")) ?? [];
  const sel = h("select", { onchange: () => call("settings:set", { agentModel: (sel as HTMLSelectElement).value }) }, h("option", { value: "" }, "auto (any live model)"), models.map((m) => h("option", { value: m }, m))) as HTMLSelectElement;
  sel.value = info?.settings.agentModel ?? "";
  const input = h("textarea", { placeholder: "Ask the agent. It can use your MCP servers and skills (Ctrl+Enter to send)." }) as HTMLTextAreaElement;
  const sendIt = async () => {
    const t = input.value.trim();
    if (!t) return;
    input.value = "";
    await call("agent:send", t);
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) sendIt();
  });
  const bySource = new Map<string, number>();
  for (const t of tools) bySource.set(t.source, (bySource.get(t.source) ?? 0) + 1);
  agentStatus = h("span", { class: "dim" });
  main.append(
    h(
      "div",
      { class: "row spread" },
      h("h1", {}, "Agent"),
      h("div", { class: "row" }, sel, btn("New chat", async () => {
        await call("agent:reset");
        transcript.replaceChildren();
      }), btn("Stop", () => call("agent:stop"))),
    ),
    h("p", { class: "dim" }, `Runs on c0mpute (${models.length} models live) with ${tools.length} tools: `, [...bySource].map(([s, n]) => h("span", { class: "tag" }, `${s} ${n}`)), " Tools ask before running unless you allowed their server."),
    transcript,
    h("div", { class: "composer" }, input, h("div", {}, btn("Send", sendIt, "b primary"), h("div", {}, agentStatus))),
  );
  if (!info?.settings) return;
}

// ── node ───────────────────────────────────────────────────────────────

async function nodeView() {
  const r = await call("node:status");
  const info = await call("app:info");
  main.append(h("h1", {}, "Node"));
  if (!r?.status) {
    main.append(h("div", { class: "card" }, h("p", { class: "bad" }, r?.error ?? "no status"), h("p", { class: "dim" }, `CLI: ${r?.cli?.path ?? "not found"} (${r?.cli?.source})`)));
    return;
  }
  const s = r.status;
  const w = info.settings.worker;
  const running = s.worker.running;
  const opt = (k: string, label: string) =>
    h("label", {}, h("input", { type: "checkbox", checked: w[k], onchange: (e: Event) => ((w[k] = (e.target as HTMLInputElement).checked), call("settings:set", { worker: w })) }), label);
  const roles = h("input", { value: w.roles, placeholder: "default roles", onchange: (e: Event) => ((w.roles = (e.target as HTMLInputElement).value), call("settings:set", { worker: w })) });
  const nick = h("input", { value: s.name ?? "", placeholder: "nickname (a-z, 0-9, -, _)" }) as HTMLInputElement;
  main.append(
    h(
      "div",
      { class: "card" },
      h("div", { class: "row spread" }, h("div", {}, h("b", { class: running ? "ok" : "dim" }, running ? `● running (pid ${s.worker.pid})` : "○ stopped"), s.worker.stale_pid ? h("span", { class: "warn" }, " (stale pid file)") : null), h("div", { class: "row" }, running ? btn("Stop", async () => (await call("node:stop"), show("node")), "b danger") : btn("Start", async () => (toast((await call("node:start")) ?? ""), show("node")), "b primary"), btn("Run as a service", async () => toast((await call("node:installService")) ?? "")))),
      h("dl", { class: "kv", style: "margin-top:12px" },
        h("dt", {}, "version"), h("dd", {}, `c0mpute ${s.version}`, h("span", { class: "dim" }, ` (${r.cli.source}: ${r.cli.path})`)),
        h("dt", {}, "peer id"), h("dd", { class: "mono" }, s.peer_id ?? "no identity yet (start the worker)"),
        h("dt", {}, "roles"), h("dd", {}, s.roles.length ? s.roles.join(", ") : "default"),
        h("dt", {}, "storage / hosting"), h("dd", {}, `${s.storage ? "on" : "off"} / ${s.hosting ? "on" : "off"}`),
        h("dt", {}, "private mode"), h("dd", {}, s.private ? "on (own buyers only)" : "off"),
        h("dt", {}, "gateway"), h("dd", { class: "mono" }, s.gateway_bind),
        h("dt", {}, "bench"), h("dd", {}, s.bench ? `score ${s.bench.score}${s.bench.quick ? " (quick, not advertised)" : ""}` : "not run"),
        h("dt", {}, "payments"), h("dd", {}, s.payments.enabled ? `${s.payments.rail}, key in $${s.payments.api_key_env} ${s.payments.api_key_set ? "(set)" : "(NOT set in this environment)"}` : "off"),
        h("dt", {}, "auto-update"), h("dd", {}, s.auto_update ? "on" : "off"),
      ),
      h("h2", {}, "Start options"),
      h("div", { class: "row" }, opt("noStorage", "no storage"), opt("noHosting", "no hosting"), opt("private", "private (this run)"), h("label", {}, "roles", roles)),
      h("h2", {}, "Nickname"),
      h("div", { class: "row" }, nick, btn("Save", async () => toast((await call("node:setName", nick.value.trim())) ?? "")), h("label", {}, h("input", { type: "checkbox", checked: s.private, onchange: async (e: Event) => toast((await call("node:setPrivate", (e.target as HTMLInputElement).checked)) ?? "") }), "private mode (permanent)")),
      h("h2", {}, "CLI updates"),
      h("div", { class: "row" }, btn("Check", async () => toast((await call("node:checkUpdate")) ?? "")), r.cli.source !== "bundled" ? btn("Update CLI", async () => toast((await call("node:update")) ?? "")) : h("span", { class: "dim" }, "The bundled CLI updates with the app.")),
    ),
  );
  // config
  const cfg = await call("node:config");
  if (cfg) {
    const st = cfg.settings;
    const f: Record<string, HTMLInputElement> = {};
    const cb = (k: string, label: string) => h("label", {}, (f[k] = h("input", { type: "checkbox", checked: st[k] }) as HTMLInputElement), label);
    const tx = (k: string, label: string, ph = "") => h("label", { class: "stack" }, label, (f[k] = h("input", { value: st[k], placeholder: ph }) as HTMLInputElement));
    const raw = h("textarea", { rows: 18 }, cfg.text) as HTMLTextAreaElement;
    main.append(
      h(
        "div",
        { class: "card" },
        h("div", { class: "row spread" }, h("b", {}, "config.toml"), h("span", { class: "dim mono" }, cfg.path)),
        h("div", { class: "row", style: "margin:10px 0" }, cb("storage", "storage provider"), cb("hosting", "hold sites"), cb("private", "private mode"), cb("autoUpdate", "auto-update"), cb("paymentsEnabled", "payments")),
        h("div", { class: "grid" }, tx("gatewayBind", "gateway bind", "0.0.0.0:7777"), tx("apiKeyEnv", "CoinPay key env var (name only)", "C0MPUTE_COINPAY_PAYOUT_KEY"), tx("recipientEmail", "payout records email"), tx("payoutWallet", "hosting payout wallet"), tx("payoutCoin", "payout coin")),
        h("p", { class: "dim" }, "The CoinPay API key itself never goes in config.toml: set it in the environment variable named above. Restart the worker to apply."),
        btn("Save settings", async () => {
          const patch: Record<string, unknown> = { paymentsRail: "coinpay" };
          for (const [k, el] of Object.entries(f)) patch[k] = el.type === "checkbox" ? el.checked : el.value.trim();
          try {
            await c0.invoke("node:saveSettings", patch);
            toast("saved; restart the worker to apply");
            show("node");
          } catch (e) {
            toast(e instanceof Error ? e.message : String(e), true);
          }
        }, "b primary"),
        h("details", { style: "margin-top:12px" }, h("summary", {}, "Edit raw config.toml"), raw, btn("Save raw", async () => {
          await call("node:saveRawConfig", raw.value);
          toast("saved");
        })),
      ),
    );
  }
  // logs
  const log = h("pre", { class: "log" });
  const refresh = async () => {
    const lines: string[] = (await call("node:logs")) ?? [];
    const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 20;
    log.textContent = lines.join("\n") || "(no worker log yet)";
    if (atBottom) log.scrollTop = log.scrollHeight;
  };
  main.append(h("div", { class: "card" }, h("b", {}, "Worker log "), h("span", { class: "dim mono" }, s.worker.log_file), log));
  await refresh();
  log.scrollTop = log.scrollHeight;
  const t = setInterval(refresh, 3000);
  cleanup.push(() => clearInterval(t));
}

// ── hosting ────────────────────────────────────────────────────────────

function jobCard(j: Any) {
  return h(
    "div",
    { class: "card", "data-job": j.id },
    h("div", { class: "row spread" }, h("b", {}, `${j.kind === "rm" ? "rm" : "publish"} ${j.name}`), h("span", { class: `tag ${j.state === "done" ? "ok" : j.state === "failed" ? "bad" : ""}` }, j.state)),
    j.dir ? h("div", { class: "dim mono" }, j.dir, j.disks ? ` --disks ${j.disks}` : "") : null,
    h("div", { style: "margin:8px 0" }, bar(j.progress?.overall)),
    h("div", { class: "mono dim" }, j.progress?.text ?? ""),
    j.url ? h("div", {}, h("a", { href: "#", onclick: (e: Event) => (e.preventDefault(), call("app:openExternal", j.url)) }, j.url)) : null,
    j.state === "running" ? btn("Cancel", () => call("hosting:cancel", j.id), "b danger") : null,
    h("details", {}, h("summary", { class: "dim" }, "output"), h("pre", { class: "log" }, (j.lines ?? []).slice(-80).join("\n"))),
  );
}

async function hostingView() {
  main.append(h("h1", {}, "Hosting"));
  // publish
  let dir = "";
  const dirEl = h("span", { class: "mono dim" }, "no folder chosen");
  const name = h("input", { placeholder: "site name (a-z, 0-9, -)" }) as HTMLInputElement;
  const disks = h("select", {}, h("option", { value: "" }, "free (3 network copies)"), h("option", { value: "auto" }, "--disks auto (paid hosts)")) as HTMLSelectElement;
  main.append(
    h(
      "div",
      { class: "card" },
      h("b", {}, "Publish a folder"),
      h("div", { class: "row", style: "margin-top:10px" }, btn("Choose folder", async () => {
        const d = await call("hosting:pickDir");
        if (d) {
          dir = d;
          dirEl.textContent = d;
          if (!name.value) name.value = d.split(/[\\/]/).pop()!.toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 63);
        }
      }), dirEl),
      h("div", { class: "row", style: "margin-top:10px" }, name, disks, btn("Publish", async () => {
        if (!dir) return toast("choose a folder first", true);
        await call("hosting:publish", dir, name.value.trim(), disks.value || null);
        show("hosting");
      }, "b primary")),
    ),
  );
  // jobs
  const jobsBox = h("div");
  const jobs: Any[] = (await call("hosting:jobs")) ?? [];
  jobsBox.append(...jobs.map(jobCard));
  main.append(h("h2", {}, "Publishes from this app"), jobsBox, jobs.length ? "" : h("p", { class: "dim" }, "none yet"));
  const off = c0.on("hosting:update", (j: Any) => {
    const old = jobsBox.querySelector(`[data-job="${j.id}"]`);
    const nu = jobCard(j);
    if (old) old.replaceWith(nu);
    else jobsBox.prepend(nu);
  });
  cleanup.push(off);
  // my sites
  const sites = await call("hosting:mySites");
  main.append(
    h("h2", {}, "Sites published from this machine"),
    h(
      "div",
      { class: "card" },
      sites?.sites?.length
        ? h("table", {}, h("tr", {}, h("th", {}, "name"), h("th", {}, "site"), h("th", {}, "copies"), h("th", {}, "size"), h("th", {})),
            sites.sites.map((s: Any) => h("tr", {}, h("td", {}, s.name), h("td", { class: "mono" }, s.site_id), h("td", {}, `${s.copies}/${sites.target_copies}`), h("td", {}, `${(s.size / 1048576).toFixed(1)} MiB`), h("td", {}, btn("rm", async () => {
              if (confirm(`Take ${s.name} down? Holders delete it and gateways answer 404.`)) {
                await call("hosting:rm", s.name);
                show("hosting");
              }
            }, "b danger")))))
        : h("p", { class: "dim" }, sites ? "No sites from this machine's key on the local gateway (is the worker running?)." : "Could not ask the local gateway."),
    ),
  );
  // chat connect bridge
  const st = await call("connect:state");
  const info = await call("app:info");
  const nodeName = h("input", { value: st?.name || info?.settings.connectHosting.name || "", placeholder: "node name (default: hostname)" }) as HTMLInputElement;
  const log = h("pre", { class: "log" }, (st?.lines ?? []).join("\n"));
  const toggle = h("input", { type: "checkbox", checked: st?.enabled }) as HTMLInputElement;
  toggle.addEventListener("change", () => call("connect:set", toggle.checked, nodeName.value.trim()));
  main.append(
    h("h2", {}, "/push hosting from the chat"),
    h("div", { class: "card" }, h("p", { class: "dim" }, "Keeps `c0mpute chat connect hosting` running so `/push hosting <name>` in the chat publishes on this machine (signed by this node, paid from its CoinPay wallet)."), h("div", { class: "row" }, h("label", {}, toggle, "connected"), nodeName, h("span", { class: st?.running ? "ok" : "dim" }, st?.running ? "running" : "not running")), log),
  );
  cleanup.push(c0.on("connect:update", (s: Any) => (log.textContent = s.lines.join("\n"))));
}

// ── tasks ──────────────────────────────────────────────────────────────

async function tasksView() {
  main.append(h("h1", {}, "Tasks"));
  const box = h("div");
  main.append(box);
  const render = async () => {
    const [ns, gw, jobs, media, acct, bridge] = await Promise.all([
      c0.invoke("node:status").catch(() => null),
      c0.invoke("node:gateway").catch(() => null),
      c0.invoke("hosting:jobs").catch(() => []),
      c0.invoke("tasks:media").catch(() => []),
      c0.invoke("account:state").catch(() => null),
      c0.invoke("connect:state").catch(() => null),
    ]);
    const [nodes, pushes] = acct?.signedIn ? await Promise.all([c0.invoke("account:nodes").catch(() => null), c0.invoke("account:pushes").catch(() => null)]) : [null, null];
    const s = ns?.status;
    box.replaceChildren(
      h("h2", {}, "This node"),
      h(
        "div",
        { class: "card" },
        h("dl", { class: "kv" },
          h("dt", {}, "worker"), h("dd", {}, s ? (s.worker.running ? `running (pid ${s.worker.pid}), roles ${s.roles.join(", ") || "default"}` : "stopped") : "unknown"),
          h("dt", {}, "hosting replication"), h("dd", {}, gw?.held != null ? `${gw.held} site(s) known to the local gateway` : "gateway not answering"),
          h("dt", {}, "storage"), h("dd", { class: "mono" }, gw?.storage ? JSON.stringify(gw.storage).slice(0, 300) : "-"),
          h("dt", {}, "/push bridge"), h("dd", {}, bridge?.enabled ? (bridge.running ? `connected as ${bridge.name || "(hostname)"}` : "restarting") : "off"),
        ),
        jobs.filter((j: Any) => j.state === "running").map(jobCard),
        h("p", { class: "dim" }, "Inference, image, music, video and training jobs this node serves show in the worker log (Node tab)."),
      ),
      h("h2", {}, "This account"),
      acct?.signedIn
        ? h(
            "div",
            { class: "card" },
            acct.me?.balance ? h("p", {}, `Spendable $${acct.me.balance.spendable_usd} (earned by your nodes $${acct.me.balance.earned_spendable_usd}, prepaid $${acct.me.balance.prepaid_available_usd})`) : null,
            h("b", {}, "Hosting nodes"),
            nodes?.nodes?.length ? h("table", {}, nodes.nodes.map((n: Any) => h("tr", {}, h("td", {}, n.name), h("td", { class: "mono dim" }, n.peer_id ?? ""), h("td", {}, n.online === false ? "offline" : n.last_seen ? `seen ${n.last_seen}` : "")))) : h("p", { class: "dim" }, "none linked"),
            h("b", {}, "/push jobs"),
            pushes?.pushes?.length
              ? pushes.pushes.slice(0, 20).map((p: Any) => h("div", { style: "margin:6px 0" }, h("div", { class: "row spread" }, h("span", {}, `${p.name} → ${p.node ?? p.node_name ?? ""}`), h("span", { class: "tag" }, p.status ?? p.state ?? "")), p.progress ? h("div", { class: "mono dim" }, typeof p.progress === "string" ? p.progress : JSON.stringify(p.progress)) : null, p.url ? h("div", { class: "mono" }, p.url) : null))
              : h("p", { class: "dim" }, "none"),
          )
        : h("div", { class: "card" }, h("p", { class: "dim" }, "Sign in (Account tab) to see your account's jobs.")),
      h("h2", {}, "Media jobs from the chat"),
      h(
        "div",
        { class: "card" },
        media.length
          ? h("table", {}, media.map((m: Any) => h("tr", {}, h("td", {}, m.kind), h("td", { class: "mono dim" }, m.id), h("td", { style: "width:30%" }, bar(m.progress ?? (m.status === "done" ? 1 : 0))), h("td", {}, h("span", { class: `tag ${m.status === "done" ? "ok" : m.status === "failed" ? "bad" : ""}` }, m.status), " ", m.detail ?? ""), h("td", { class: "dim" }, ago(m.at)))))
          : h("p", { class: "dim" }, "Images, songs, albums and promo videos you start in the chat show up here."),
      ),
    );
  };
  await render();
  const t = setInterval(render, 5000);
  cleanup.push(() => clearInterval(t), c0.on("tasks:update", render));
}

// ── MCP servers ────────────────────────────────────────────────────────

function kvEditor(obj: Record<string, string> | undefined, keyPh: string) {
  const rows = h("div");
  const addRow = (k = "", v = "") => rows.append(h("div", { class: "row", style: "margin:4px 0" }, h("input", { value: k, placeholder: keyPh, "data-k": 1 }), h("input", { value: v, placeholder: "value or ${VAR}", "data-v": 1, style: "flex:1" }), h("button", { class: "b", type: "button" }, "×")));
  for (const [k, v] of Object.entries(obj ?? {})) addRow(k, v);
  rows.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    if (t.textContent === "×") t.parentElement?.remove();
  });
  const el = h("div", {}, rows, btn("+ add", () => addRow()));
  const read = () => {
    const out: Record<string, string> = {};
    rows.querySelectorAll(".row").forEach((r) => {
      const k = (r.querySelector("[data-k]") as HTMLInputElement).value.trim();
      const v = (r.querySelector("[data-v]") as HTMLInputElement).value;
      if (k) out[k] = v;
    });
    return out;
  };
  return { el, read };
}

function editServer(s: Any | null, after: () => void) {
  const isNew = !s;
  s = s ?? { name: "", transport: "stdio", command: "", args: [], env: {}, url: "", headers: {} };
  const name = h("input", { value: s.name, placeholder: "name" }) as HTMLInputElement;
  const transport = h("select", {}, ["stdio", "http", "sse"].map((t) => h("option", { value: t }, t))) as HTMLSelectElement;
  transport.value = s.transport;
  const command = h("input", { value: s.command ?? "", placeholder: "command, e.g. npx" }) as HTMLInputElement;
  const args = h("input", { value: (s.args ?? []).join(" "), placeholder: "args (space separated)", style: "flex:1" }) as HTMLInputElement;
  const url = h("input", { value: s.url ?? "", placeholder: "https://example.com/mcp", style: "flex:1" }) as HTMLInputElement;
  const env = kvEditor(s.env, "ENV_NAME");
  const headers = kvEditor(s.headers, "Header");
  const stdioBox = h("div", {}, h("div", { class: "row" }, command, args), h("h2", {}, "env"), env.el);
  const httpBox = h("div", {}, h("div", { class: "row" }, url), h("h2", {}, "headers"), headers.el);
  const sync = () => {
    stdioBox.hidden = transport.value !== "stdio";
    httpBox.hidden = transport.value === "stdio";
  };
  transport.addEventListener("change", sync);
  sync();
  const close = modal(
    h(
      "div",
      {},
      h("h1", {}, isNew ? "Add MCP server" : `Edit ${s.name}`),
      h("div", { class: "row" }, name, transport),
      stdioBox,
      httpBox,
      h("p", { class: "dim" }, "Use ${VAR} for secrets: the value comes from the environment or the app's encrypted store and is never written to the file. Secret values already in the file show masked; leave them masked to keep them."),
      h("div", { class: "row" }, btn("Save", async () => {
        const server = {
          name: name.value.trim(),
          transport: transport.value,
          command: command.value.trim(),
          args: args.value.trim() ? args.value.trim().split(/\s+/) : [],
          env: env.read(),
          url: url.value.trim(),
          headers: headers.read(),
          disabled: s.disabled ?? false,
          file: s.file ?? "",
        };
        if ((await call("mcp:save", server, isNew ? undefined : s.name)) !== undefined) {
          close();
          after();
        }
      }, "b primary"), btn("Cancel", () => close())),
    ),
  );
}

async function importClaude() {
  const list: Any[] = (await call("mcp:claudePreview")) ?? [];
  if (!list.length) return toast("No MCP servers found in ~/.claude.json, ~/.mcp.json or Claude Desktop's config.");
  const boxes = list.map((s) => ({ s, cb: h("input", { type: "checkbox", checked: !s.exists, disabled: s.exists }) as HTMLInputElement }));
  const close = modal(
    h(
      "div",
      {},
      h("h1", {}, "Import from Claude Code / Claude Desktop"),
      h("table", {}, boxes.map(({ s, cb }) => h("tr", {}, h("td", {}, cb), h("td", {}, s.name), h("td", { class: "dim" }, s.transport), h("td", { class: "dim mono" }, s.url ?? `${s.command} ${(s.args ?? []).join(" ")}`), h("td", { class: "dim" }, s.exists ? "already here" : s.note)))),
      h("div", { class: "row", style: "margin-top:12px" }, btn("Import", async () => {
        const r = await call("mcp:claudeImport", boxes.filter((b) => b.cb.checked).map((b) => b.s.name));
        if (r) toast(`imported ${r.added.length}${r.skipped.length ? `, skipped ${r.skipped.join(", ")}` : ""}`);
        close();
        show("mcp");
      }, "b primary"), btn("Cancel", () => close())),
    ),
  );
}

async function setVar(name: string) {
  const input = h("input", { type: "password", placeholder: `value for ${name}`, style: "width:100%" }) as HTMLInputElement;
  const close = modal(h("div", {}, h("h1", {}, `Set ${name}`), h("p", { class: "dim" }, "Stored encrypted with your OS keychain (safeStorage), only used when connecting. An environment variable of the same name wins."), input, h("div", { class: "row", style: "margin-top:12px" }, btn("Save", async () => {
    const r = await call("mcp:setSecret", name, input.value);
    if (r && !r.encrypted) toast("No keychain available: stored in a 0600 file instead.", true);
    close();
    show("mcp");
  }, "b primary"), btn("Cancel", () => close()))));
  input.focus();
}

async function mcpView() {
  const v = await call("mcp:list");
  if (!v) return;
  main.append(
    h("div", { class: "row spread" }, h("h1", {}, "MCP servers"), h("div", { class: "row" }, btn("Add server", () => editServer(null, () => show("mcp")), "b primary"), btn("Import from Claude", importClaude), btn("Reload", async () => (await call("mcp:reload"), show("mcp"))), btn("Open folder", () => call("app:openPath", v.dir)))),
    h("p", { class: "dim" }, `From ${v.dir}/*.json ({"mcpServers": {...}}, the Claude Code / Claude Desktop shape). The Agent tab can use every enabled server's tools.`),
    v.errors.length ? h("div", { class: "card" }, v.errors.map((e: Any) => h("div", { class: "warn" }, `${e.file.split("/").pop()}: ${e.error}`))) : "",
  );
  for (const s of v.servers) {
    const stateCls = s.state === "ready" ? "ok" : s.state === "error" ? "bad" : "";
    main.append(
      h(
        "div",
        { class: "card" },
        h("div", { class: "row spread" },
          h("div", {}, h("b", {}, s.name), " ", h("span", { class: "tag" }, s.transport), " ", h("span", { class: `tag ${stateCls}` }, s.state === "ready" ? `${s.tools.length} tools` : s.state)),
          h("div", { class: "row" },
            h("label", {}, h("input", { type: "checkbox", checked: !s.disabled, onchange: async (e: Event) => (await call("mcp:toggle", s.name, (e.target as HTMLInputElement).checked), show("mcp")) }), "enabled"),
            h("label", { title: "Run this server's tools without asking each time" }, h("input", { type: "checkbox", checked: s.allowed, onchange: (e: Event) => call("mcp:allow", s.name, (e.target as HTMLInputElement).checked) }), "no confirm"),
            btn("Edit", () => editServer(s, () => show("mcp"))),
            btn("Reconnect", async () => (await call("mcp:reconnect", s.name), show("mcp"))),
            btn("Remove", async () => {
              if (confirm(`Remove ${s.name} from ${s.file}?`)) (await call("mcp:delete", s.name), show("mcp"));
            }, "b danger"),
          ),
        ),
        s.note ? h("div", { class: "dim" }, s.note) : null,
        h("div", { class: "mono dim" }, s.transport === "stdio" ? `${s.command} ${(s.args ?? []).join(" ")}` : s.url),
        Object.keys({ ...(s.env ?? {}), ...(s.headers ?? {}) }).length ? h("div", { class: "mono dim" }, Object.entries({ ...(s.env ?? {}), ...(s.headers ?? {}) }).map(([k, val]) => `${k}=${val}`).join("  ")) : null,
        s.vars.length ? h("div", { class: "row", style: "margin-top:6px" }, s.vars.map((x: Any) => h("span", {}, h("span", { class: `tag ${x.set ? "ok" : "bad"}` }, `\${${x.name}} ${x.set ?? "missing"}`), " ", btn("set", () => setVar(x.name))))) : null,
        s.error ? h("div", { class: "bad" }, s.error) : null,
        s.tools.length ? h("details", {}, h("summary", { class: "dim" }, "tools"), h("ul", {}, s.tools.map((t: Any) => h("li", {}, h("b", {}, t.name), " ", h("span", { class: "dim" }, t.description.slice(0, 200)))))) : null,
        s.stderr?.length ? h("details", {}, h("summary", { class: "dim" }, "stderr"), h("pre", { class: "log" }, s.stderr.join("\n"))) : null,
      ),
    );
  }
  cleanup.push(c0.on("mcp:update", () => tab === "mcp" && show("mcp")));
}

// ── skills ─────────────────────────────────────────────────────────────

async function skillsView() {
  const v = await call("skills:list");
  if (!v) return;
  const name = h("input", { placeholder: "new-skill-name" }) as HTMLInputElement;
  const desc = h("input", { placeholder: "what it does and when to use it", style: "flex:1" }) as HTMLInputElement;
  main.append(
    h("div", { class: "row spread" }, h("h1", {}, "Skills"), h("div", { class: "row" }, btn("Import folder", async () => (await call("skills:import", "folder")) && show("skills")), btn("Import zip", async () => (await call("skills:import", "zip")) && show("skills")), btn("Open folder", () => call("app:openPath", v.dir)))),
    h("p", { class: "dim" }, `Agent Skills from ${v.dir} (one folder per skill, SKILL.md with name + description). The agent sees each enabled skill's description and loads the rest when it needs it.`),
    h("div", { class: "card" }, h("div", { class: "row" }, name, desc, btn("Create from template", async () => (await call("skills:create", name.value.trim(), desc.value.trim())) && show("skills"), "b primary"))),
    h(
      "div",
      { class: "card" },
      h("label", {}, h("input", { type: "checkbox", checked: v.scriptsAllowed, onchange: (e: Event) => call("settings:set", { skillScriptsAllowed: (e.target as HTMLInputElement).checked }) }), "run skill scripts without asking"),
      h("label", {}, h("input", { type: "checkbox", checked: v.shellEnabled, onchange: (e: Event) => call("settings:set", { shellEnabled: (e.target as HTMLInputElement).checked }) }), "give the agent a shell tool (every command is confirmed)"),
    ),
  );
  for (const s of v.skills) {
    main.append(
      h(
        "div",
        { class: "card" },
        h("div", { class: "row spread" }, h("div", {}, h("b", {}, s.name), s.error ? h("span", { class: "tag bad" }, s.error) : null), h("div", { class: "row" }, h("label", {}, h("input", { type: "checkbox", checked: s.enabled, onchange: (e: Event) => call("skills:toggle", s.dir.split(/[\\/]/).pop(), (e.target as HTMLInputElement).checked) }), "enabled"), btn("Open SKILL.md", () => call("skills:open", s.name)))),
        h("div", { class: "dim" }, s.description),
        s.scripts.length ? h("div", { class: "mono dim" }, `scripts: ${s.scripts.join(", ")}`) : null,
      ),
    );
  }
}

// ── account / settings ─────────────────────────────────────────────────

async function settingsView() {
  const [acct, info] = await Promise.all([call("account:state"), call("app:info")]);
  const loginLog = h("pre", { class: "log" });
  loginLog.hidden = true;
  const off = c0.on("account:login", (p: Any) => {
    loginLog.hidden = false;
    if (p.line) loginLog.textContent += `${p.line}\n`;
    if (p.done) {
      toast(p.ok ? "signed in" : "sign-in did not finish", !p.ok);
      show("settings");
    }
  });
  cleanup.push(off);
  main.append(
    h("h1", {}, "Account"),
    h(
      "div",
      { class: "card" },
      acct?.signedIn
        ? h("div", {}, h("p", {}, "Signed in", acct.me?.name ? ` as ${acct.me.name}` : "", acct.me?.did ? h("span", { class: "dim mono" }, ` ${acct.me.did}`) : null), acct.me?.balance ? h("p", {}, `Spendable $${acct.me.balance.spendable_usd}, earned by your nodes $${acct.me.balance.earned_spendable_usd}`) : null, acct.error ? h("p", { class: "bad" }, acct.error) : null, h("p", { class: "dim" }, `key from ${acct.source === "env" ? "$C0MPUTE_API_KEY" : info?.chatJson}`), btn("Sign out", async () => (toast((await call("account:logout")) ?? ""), show("settings")), "b danger"))
        : h("div", {}, h("p", {}, "Not signed in. Sign-in uses `c0mpute chat login`: approve the code on c0mpute.com with CoinPay (a DID is required). The key is kept by the CLI in ~/.c0mpute/chat.json."), btn("Sign in with CoinPay", () => call("account:login"), "b primary")),
      loginLog,
    ),
    h("h1", {}, "App"),
    h(
      "div",
      { class: "card" },
      h("dl", { class: "kv" }, h("dt", {}, "version"), h("dd", {}, info?.version), h("dt", {}, "CLI"), h("dd", { class: "mono" }, `${info?.cli.path ?? "not found"} (${info?.cli.source})`), h("dt", {}, "platform"), h("dd", {}, `${info?.platform} ${info?.arch}${info?.appImage ? " (AppImage)" : ""}`), h("dt", {}, "auto-update"), h("dd", {}, info?.noAutoUpdate ? "off (C0MPUTE_NO_AUTO_UPDATE=1)" : "on")),
      h("div", { class: "row", style: "margin-top:12px" },
        btn("Check for updates", async () => {
          const r = await call("app:checkUpdate");
          toast(!r ? "" : r.kind === "none" ? "up to date" : r.kind === "disabled" ? "updates are off (C0MPUTE_NO_AUTO_UPDATE)" : r.kind === "installed" ? `${r.version} installed: restart to use it` : `${r.version} is out: ${r.url}`);
        }),
        btn("Install the `c0mpute` command", async () => {
          const r = await call("cli:installShim");
          if (r) toast(r.action === "skip" ? r.reason : `wrote ${r.shim} → ${r.target}`);
        }),
      ),
    ),
  );
}

const views: Record<string, () => unknown> = { chat: chatView, agent: agentView, node: nodeView, hosting: hostingView, tasks: tasksView, mcp: mcpView, skills: skillsView, settings: settingsView };

// node pill
async function pill() {
  const r = await c0.invoke("node:status").catch(() => null);
  const p = $("#nodepill");
  const up = r?.status?.worker.running;
  p.textContent = r?.status ? (up ? "● node running" : "○ node stopped") : "node: no CLI";
  p.classList.toggle("up", !!up);
}
pill();
setInterval(pill, 15_000);
show("chat");

export {};
