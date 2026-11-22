#!/usr/bin/env python3
"""Status page for the c0mpute worker container (ghcr.io/profullstack/c0mpute-worker).

A small stdlib-only web page on C0MPUTE_UI_BIND (default 0.0.0.0:8080) so a
container on a home server (Umbrel, CasaOS, Unraid) can be checked and set up
from the browser: whether the worker is running, its node id and roles, and
forms to set the public nickname and the payout address for inference
earnings. It shells out to the same CLIs an operator would type.

There is no login: put it behind your platform's auth (Umbrel's app proxy
does this) or do not publish the port. Set C0MPUTE_UI=0 to turn it off.
"""

import html
import json
import os
import re
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

NAME_RE = re.compile(r"^[a-z0-9_-]{1,32}$")
COIN_RE = re.compile(r"^[A-Za-z0-9_]{2,16}$")
ADDRESS_RE = re.compile(r"^[A-Za-z0-9:._-]{16,128}$")
NETWORK_RE = re.compile(r"^[A-Za-z0-9_-]{0,32}$")
EMAIL_RE = re.compile(r"^[^@\s<>\"']{1,64}@[^@\s<>\"']{1,190}$")


def run(args, timeout=20):
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except (OSError, subprocess.TimeoutExpired) as e:
        return 1, str(e)


def worker_status():
    # stdout only: the CLI logs to stderr, and may print a log line first.
    try:
        p = subprocess.run(["c0mpute", "worker", "status", "--json"],
                           capture_output=True, text=True, timeout=20)
        out = p.stdout
        status, _ = json.JSONDecoder().raw_decode(out[out.index("{"):])
    except (OSError, subprocess.TimeoutExpired, ValueError):
        status = {}
    # A foreground worker (the container's main process) may not write a PID
    # file, so also look for the process itself.
    pcode, _ = run(["pgrep", "-f", "c0mpute worker start"])
    status["process_running"] = pcode == 0
    return status


def payout_summary():
    code, out = run(["infernet", "payout", "list"], timeout=30)
    if code != 0:
        return "Not available yet. The node registers on its first start; try again in a few minutes."
    return out.strip()


def esc(v):
    return html.escape("" if v is None else str(v))


PAGE = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>c0mpute node</title>
<style>
:root {{ color-scheme: light dark; --fg:#111; --bg:#fafafa; --muted:#555; --card:#fff; --line:#ddd; --ok:#0a7a32; --bad:#b42318; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg:#eee; --bg:#111; --muted:#aaa; --card:#1b1b1b; --line:#333; --ok:#4ade80; --bad:#f87171; }} }}
body {{ font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); margin: 0; padding: 16px; }}
main {{ max-width: 760px; margin: 0 auto; }}
h1 {{ font-size: 22px; margin: 8px 0 4px; }}
section {{ background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 14px 16px; margin: 14px 0; }}
h2 {{ font-size: 16px; margin: 0 0 8px; }}
dl {{ display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; margin: 0; }}
dt {{ color: var(--muted); }} dd {{ margin: 0; overflow-wrap: anywhere; }}
.ok {{ color: var(--ok); font-weight: 600; }} .bad {{ color: var(--bad); font-weight: 600; }}
pre {{ white-space: pre-wrap; overflow-wrap: anywhere; margin: 0; font-size: 13px; }}
form {{ display: flex; flex-wrap: wrap; gap: 8px; align-items: end; margin: 8px 0 0; }}
label {{ display: flex; flex-direction: column; font-size: 13px; color: var(--muted); }}
input {{ font: inherit; padding: 6px 8px; border: 1px solid var(--line); border-radius: 6px; background: var(--bg); color: var(--fg); min-width: 0; }}
button {{ font: inherit; padding: 6px 14px; border-radius: 6px; border: 1px solid var(--line); cursor: pointer; }}
.note {{ color: var(--muted); font-size: 13px; }}
.msg {{ border-left: 3px solid var(--ok); padding: 6px 10px; }}
a {{ color: inherit; }}
</style></head><body><main>
<h1>c0mpute node</h1>
<p class="note">This machine is a node on <a href="https://c0mpute.com">c0mpute</a>, a decentralized compute network.
It serves inference, transcoding and storage jobs from the network. Earnings depend on demand and vary; estimates are not payouts.</p>
{message}
<section><h2>Status</h2><dl>
<dt>Worker</dt><dd>{running}</dd>
<dt>Version</dt><dd>{version}</dd>
<dt>Node id</dt><dd>{peer_id}</dd>
<dt>Nickname</dt><dd>{name}</dd>
<dt>Roles</dt><dd>{roles}</dd>
<dt>Storage / hosting</dt><dd>{storage} / {hosting}</dd>
<dt>Benchmark</dt><dd>{bench}</dd>
</dl>
<p class="note">See every node on the <a href="https://c0mpute.com/nodes">node board</a>.</p></section>

<section><h2>Earnings and payout address</h2>
<pre>{payout}</pre>
<form method="post" action="payout">
<label>Coin <input name="coin" placeholder="USDC" required pattern="[A-Za-z0-9_]{{2,16}}" size="8"></label>
<label>Network (optional) <input name="network" placeholder="solana" size="10"></label>
<label style="flex:1">Address <input name="address" required minlength="16" maxlength="128"></label>
<button>Save payout address</button>
</form>
<p class="note">Earnings accrue with or without an address; you need one to withdraw. Supported coins are listed by <code>infernet payout --help</code>.</p></section>

<section><h2>Nickname</h2>
<form method="post" action="name">
<label>Public nickname on the node board <input name="name" value="{name_value}" pattern="[a-z0-9_-]{{1,32}}" required></label>
<button>Save</button></form>
<form method="post" action="contact">
<label style="flex:1">Contact email (optional, public) <input name="email" type="email"></label>
<button>Save</button></form>
<p class="note">Restart the app for a new nickname or contact to show on the board.</p></section>
</main></body></html>
"""


def render(message=""):
    s = worker_status()
    worker = s.get("worker") or {}
    running = s.get("process_running") or worker.get("running")
    bench = s.get("bench") or {}
    return PAGE.format(
        message=f'<p class="msg">{esc(message)}</p>' if message else "",
        running='<span class="ok">running</span>' if running else '<span class="bad">not running</span>',
        version=esc(s.get("version", "unknown")),
        peer_id=esc(s.get("peer_id") or "not created yet"),
        name=esc(s.get("name") or "not set"),
        name_value=esc(s.get("name") or ""),
        roles=esc(", ".join(s.get("roles") or []) or "defaults"),
        storage="on" if s.get("storage") else "off",
        hosting="on" if s.get("hosting") else "off",
        bench=esc(f"score {bench.get('score')}" if bench.get("score") is not None else "not run yet"),
        payout=esc(payout_summary()),
    )


class Handler(BaseHTTPRequestHandler):
    server_version = "c0mpute-status"

    def log_message(self, fmt, *args):
        sys.stderr.write("status-ui: " + (fmt % args) + "\n")

    def send_page(self, body, status=200):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Frame-Options", "SAMEORIGIN")
        self.send_header("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urlsplit(self.path).path
        if path in ("/health", "/healthz"):
            self.send_page("ok")
            return
        if path not in ("/", "/index.html"):
            self.send_page("not found", 404)
            return
        self.send_page(render())

    def same_origin(self):
        origin = self.headers.get("Origin")
        if origin is None:
            return True
        return urlsplit(origin).netloc == self.headers.get("Host")

    def do_POST(self):
        if not self.same_origin():
            self.send_page("cross-origin request refused", 403)
            return
        length = min(int(self.headers.get("Content-Length") or 0), 4096)
        form = {k: v[0].strip() for k, v in parse_qs(self.rfile.read(length).decode()).items()}
        action = urlsplit(self.path).path.rstrip("/").rsplit("/", 1)[-1]
        if action == "payout":
            coin, address, network = form.get("coin", ""), form.get("address", ""), form.get("network", "")
            if not (COIN_RE.match(coin) and ADDRESS_RE.match(address) and NETWORK_RE.match(network)):
                msg = "That coin, network or address does not look right."
            else:
                args = ["infernet", "payout", "set", coin.upper(), address]
                if network:
                    args += ["--network", network]
                code, out = run(args, timeout=30)
                msg = "Payout address saved." if code == 0 else "Could not save it: " + out.strip()[-300:]
        elif action == "name":
            name = form.get("name", "")
            if not NAME_RE.match(name):
                msg = "Nicknames are 1-32 of a-z, 0-9, - and _."
            else:
                code, out = run(["c0mpute", "name", name])
                msg = "Nickname saved; restart the app to publish it." if code == 0 else out.strip()[-300:]
        elif action == "contact":
            email = form.get("email", "")
            if email and not EMAIL_RE.match(email):
                msg = "That email does not look right."
            else:
                args = ["c0mpute", "contact", email] if email else ["c0mpute", "contact", "--clear"]
                code, out = run(args)
                msg = "Contact saved; restart the app to publish it." if code == 0 else out.strip()[-300:]
        else:
            self.send_page("not found", 404)
            return
        self.send_page(render(msg))


def main():
    bind = os.environ.get("C0MPUTE_UI_BIND", "0.0.0.0:8080")
    host, _, port = bind.rpartition(":")
    server = ThreadingHTTPServer((host or "0.0.0.0", int(port)), Handler)
    sys.stderr.write(f"status-ui: listening on {bind}\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
