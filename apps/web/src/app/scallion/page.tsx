import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

const REPO = "https://github.com/profullstack/c0mpute";
const DOCS = `${REPO}/blob/master/docs/protocol`;
const NET = `${REPO}/blob/master/node/crates/c0mpute-net/src`;

export const metadata = {
  title: "scallion network (VPN, experimental) — c0mpute",
  description:
    "Scallion Network is c0mpute's experimental privacy routing layer: a three-hop route planner, an opt-in exit policy, layered encrypted hop frames, and two client modes (native and OpenVPN Connect). It is a foundation, not a working VPN yet: this page says exactly what is built and what is not.",
  alternates: { canonical: "https://c0mpute.com/scallion" },
};

export default function ScallionPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">scallion network</h1>
        <p className="comment">// green routes, private roots: three-hop privacy routing with native and OpenVPN client modes</p>
      </header>

      <section className="border border-[var(--color-accent-dim)] rounded-md px-4 py-3">
        <p className="text-sm leading-6 text-[var(--color-fg)]">
          Status: experimental foundation. You cannot connect to a VPN with
          c0mpute today.
        </p>
        <p className="text-xs text-[var(--color-dim)] mt-1 leading-6">
          The <code>c0mpute</code> binary does not redirect your traffic, open
          a TUN device, run an OpenVPN server or hand out a working{" "}
          <code>.ovpn</code> file, and nothing here makes you anonymous. What
          has shipped is the route planner, the exit policy, the hop
          encryption primitive, the client mode contracts and a diagnostics
          command that tells you all of this itself. We would rather say that
          plainly than let a page imply working code.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what it is ]</h2>
        <p>
          Scallion (a green onion) is a general-purpose privacy layer for the
          c0mpute network: traffic for browsing, APIs, terminals and compute
          jobs is meant to travel through three independently operated nodes
          (an entry guard, a middle and an opt-in exit), each of which can
          open only its own layer of encryption. Joining needs nothing but
          the keypair your node already generates; there is no registration.
        </p>
        <p>
          It is not Tor and does not claim to be. The libp2p relays every node
          runs today are for reaching peers behind NAT: a relay knows both
          ends of the connection it carries, so they are not privacy
          circuits.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ two client modes ]</h2>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row
              k="native"
              v="the c0mpute CLI and future desktop and mobile apps: a local Scallion TUN device, authenticated three-hop circuits, an opt-in exit. Not implemented yet."
            />
            <Row
              k="openvpn"
              v="stock OpenVPN Connect on phones and desktops, through an operator-opted-in OpenVPN-protocol entry gateway that bridges into the same three-hop circuit. Not implemented yet."
            />
          </tbody>
        </table>
        <p>
          The modes are independent. Native needs no OpenVPN. OpenVPN Connect
          needs a real OpenVPN-compatible entry gateway, because a libp2p peer
          cannot accept a <code>.ovpn</code> connection directly, and no node
          becomes a gateway unless its operator opts in. Neither mode may
          fall back to a direct connection or to the other mode: if a required
          piece is missing, the client refuses to connect.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ check it yourself ]</h2>
        <p>
          Since v0.3.78 the CLI reports the real readiness of each mode. These
          commands start nothing, open no listener and change no routes.
        </p>
        <CodeBlock>{`c0mpute scallion modes                         # both modes and their blockers
c0mpute scallion modes --json
c0mpute scallion doctor --mode native          # one mode
c0mpute scallion doctor --mode openvpn --json`}</CodeBlock>
        <p>What a current node prints:</p>
        <CodeBlock>{`native: not ready
  - audited layered-encrypted multi-hop transport is not implemented
  - consenting exit enforcement is not connected to the data plane
  - DNS, IPv4/IPv6 and fail-closed leak handling are not verified
  - multi-node adversarial end-to-end tests have not passed
  - independent security review is outstanding
  - native TUN client transport is not implemented
openvpn: not ready
  - audited layered-encrypted multi-hop transport is not implemented
  - consenting exit enforcement is not connected to the data plane
  - DNS, IPv4/IPv6 and fail-closed leak handling are not verified
  - multi-node adversarial end-to-end tests have not passed
  - independent security review is outstanding
  - opt-in OpenVPN protocol entry gateway is not implemented`}</CodeBlock>
        <p className="text-[var(--color-dim)]">
          A mode turns <code>ready</code> only when the local runtime has the
          data plane, the exits, the leak handling, the tests and the review.
          A remote node advertising the right capabilities is not enough.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what is built ]</h2>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row
              k="route planner"
              v="ranks three-hop paths from measured link RTT, loss and bandwidth (never self-reported speed); requires a pinned guard, three distinct peers, operators and ASNs, and an exit that opted in; samples among near-fastest paths so one path does not absorb all traffic. Unit-tested route decisions, not wired to live connections."
            />
            <Row
              k="sticky sessions"
              v="an established session keeps its circuit; only new streams move after a lease expires, so SSH, HQTerm, inference streams and WebSockets are not broken by rotation."
            />
            <Row
              k="exit policy"
              v="deny by default. An exit operator opts in with an allowlist of TCP ports; a destination is refused if any DNS answer is private, loopback, link-local or otherwise special, and the approved address is pinned so a second lookup cannot rebind it. Not used by a live exit yet."
            />
            <Row
              k="encrypted hop frames"
              v="three nested layers (guard, middle, exit), each with its own AES-256-GCM key from X25519 and HKDF-SHA256, with replay rejection. A unit-tested primitive that touches no socket; relay key authentication and forward secrecy are still to do."
            />
            <Row
              k="OpenVPN profile builder"
              v="formats a .ovpn for a provisioned, trusted OpenVPN server: pinned CA, certificate name and EKU checks, no embedded secrets, no directive injection. A library function only; it is not exposed as a command until a gateway exists."
            />
            <Row
              k="independent bootstrap"
              v="live. A node starts without an HTTPS check-in to c0mpute.com and can dial only peers you choose."
            />
          </tbody>
        </table>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ join without our seeds ]</h2>
        <p>
          The one part you can use today. By default a node dials the public
          libp2p seeds compiled into the binary. Community mode dials only
          the peers you supply and never falls back to ours.
        </p>
        <CodeBlock>{`# only independently operated peers
export C0MPUTE_BOOTSTRAP_MODE=community
export C0MPUTE_BOOTSTRAP_PEERS='/dns4/<host>/tcp/4001/p2p/<full-peer-id>'
c0mpute worker start

# or one full multiaddr per line in a file
export C0MPUTE_BOOTSTRAP_FILE="$HOME/.config/c0mpute/scallion-peers.txt"

# no configured WAN peers at all (LAN discovery only)
C0MPUTE_BOOTSTRAP_MODE=offline c0mpute worker start`}</CodeBlock>
        <p className="text-[var(--color-dim)]">
          A seed you dial directly still sees your IP address. Community mode
          avoids our seeds; it does not hide you from the ones you pick.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what has to happen first ]</h2>
        <ol className="list-decimal pl-5 space-y-1">
          <li>An authenticated relay handshake with forward secrecy, key rotation and circuit teardown, reviewed before it carries traffic.</li>
          <li>Hop frames carried over real peer streams through three independent nodes, with per-circuit limits.</li>
          <li>The exit role: operator consent, destination policy, quotas and abuse controls, connected to the data plane.</li>
          <li>The native TUN client, with routes, DNS and IPv6 handled per platform and a fail-closed kill switch.</li>
          <li>The opt-in OpenVPN gateway, then signed working <code>.ovpn</code> profiles for stock OpenVPN Connect.</li>
          <li>Multi-node tests under forced node and exit failures, with DNS and IP leak checks.</li>
          <li>An independent security review before any anonymity claim or public use.</li>
        </ol>
        <p>
          Exits are a separate, explicit role. Running a c0mpute compute
          worker never turns your machine into an internet exit.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ limits that will not change ]</h2>
        <p>
          A guard always sees your IP address and an exit always sees the
          destination, plus the content if the application does not use TLS.
          Someone watching both ends can correlate timing and volume. No
          account and no KYC to join is not the same as anonymous.
        </p>
        <p>
          Scallion must not be used to get around sanctions, export controls
          or identity checks a regulated service requires. Joining a
          permissionless network does not grant permission to use any
          specific service on it.
        </p>
      </section>

      <section className="space-y-2 rule pt-8 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ specification ]</h2>
        <ul className="space-y-1 text-xs">
          <li>→ <a href={`${DOCS}/scallion-network.md`}>scallion-network</a>: architecture, path scoring, session continuity, roadmap</li>
          <li>→ <a href={`${DOCS}/scallion-clients.md`}>scallion-clients</a>: native and OpenVPN client modes, profile rules, release gates</li>
          <li>→ <a href={`${DOCS}/scallion-exit-policy.md`}>scallion-exit-policy</a>: the deny-by-default exit admission contract</li>
          <li>→ <a href={`${DOCS}/scallion-frames.md`}>scallion-frames</a>: hop frame format, threat model and stated limitations</li>
          <li>→ <a href={`${DOCS}/scallion-bootstrap.md`}>scallion-bootstrap</a>: bootstrap modes and every environment variable</li>
        </ul>
        <p className="text-xs text-[var(--color-dim)] pt-2">
          The code is the <code>scallion*</code> modules of{" "}
          <a href={`${NET}/scallion.rs`}>
            <code>c0mpute-net</code>
          </a>
          . Test it with <code>cargo test -p c0mpute-net scallion</code>.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/protocol">protocol</Link> ·{" "}
          <Link href="/docs#scallion">docs</Link> ·{" "}
          <Link href="/providers">run a node</Link>
        </p>
      </section>
    </div>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <tr>
      <td className="py-2 pr-6 align-top accent whitespace-nowrap">{k}</td>
      <td className="py-2 text-[var(--color-dim)] leading-6">{v}</td>
    </tr>
  );
}
