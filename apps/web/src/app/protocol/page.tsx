import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

const REPO = "https://github.com/profullstack/c0mpute";
const DOCS = `${REPO}/blob/master/docs/protocol`;

export const metadata = {
  title: "protocol — c0mpute",
  description:
    "The c0mpute open compute protocol: libp2p transport, signed provider adverts, job manifests, offers, buyer-side scheduling, validation tiers, settlement adapters and signed receipts. c0mpute.com infrastructure is not required for the network to operate.",
  alternates: { canonical: "https://c0mpute.com/protocol" },
};

export default function ProtocolPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">protocol</h1>
        <p className="comment">// what interoperating with c0mpute actually requires</p>
      </header>

      <section className="border border-[var(--color-accent-dim)] rounded-md px-4 py-3">
        <p className="text-sm leading-6 text-[var(--color-fg)]">
          c0mpute.com infrastructure is not required for the c0mpute network
          to operate.
        </p>
        <p className="text-xs text-[var(--color-dim)] mt-1 leading-6">
          Everything below is designed so two peers who have never heard of us
          can find each other, agree a price, run work, check the result and
          settle — and so anyone holding the resulting records can verify them
          without asking us, or anyone, for permission.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ four kinds of thing ]</h2>
        <p>
          Every part of c0mpute is exactly one of these. Mixing them up is how
          a decentralized network quietly acquires a single point of failure,
          so we write it down where each component lives.
        </p>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row
              k="protocol requirement"
              v="an implementation MUST do this to interoperate"
            />
            <Row
              k="reference implementation"
              v="how we do it; another node may differ"
            />
            <Row
              k="optional hosted service"
              v="an accelerator; the network works without it"
            />
            <Row
              k="first-party integration"
              v="our product, best-supported, never mandatory"
            />
          </tbody>
        </table>
        <p>
          Indexers, gateways, dashboards, relays and the Infernet control
          plane are all in the third row. They may cache, observe, proxy and
          bill. None of them may be <em>required</em> for two honest peers to
          transact.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ identity ]</h2>
        <p>
          A peer&apos;s identity is an ed25519 keypair, rendered as a DID and
          generated locally — offline, with no account and no payment product
          in the path.
        </p>
        <CodeBlock>{`did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar`}</CodeBlock>
        <p>
          It is self-certifying: the DID <em>is</em> the public key, so
          verifying a signature needs no registry, no lookup and no network
          round trip. The encoding is byte-compatible with{" "}
          <code>did:key</code> for ed25519, so existing DID tooling resolves a
          c0mpute identity by swapping the method name.
        </p>
        <p className="text-[var(--color-dim)]">
          Wallets, KYB, hardware attestation and organization membership
          attach as separate credentials. None is required to join.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ signed envelopes ]</h2>
        <p>
          Every protocol message travels in an envelope carrying its own
          detached signature over canonical bytes.
        </p>
        <CodeBlock>{`{
  "v": 1,
  "type": "c0mpute.offer/v1",
  "signer": "did:c0mpute:z6Mksw…",
  "payload": { … },
  "sig": "z3J1bx…"
}`}</CodeBlock>
        <p>
          Why, when libp2p already signs each pubsub message? Because a
          transport signature dies at the first hop. An advert relayed by an
          indexer, a receipt read back off disk after a restart, an offer
          forwarded by a gateway, a record shown to someone who was never a
          peer — none of those carry it, and all of them need to be
          verifiable. Reputation built on unverifiable receipts has to be{" "}
          <em>believed</em>, which is what forces a trusted database into the
          middle of a network designed not to need one.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ the message types ]</h2>
        <p>
          Four signed records carry the market. Each is self-contained —
          everything needed to verify it is inside it.
        </p>
        <div className="space-y-3">
          <Msg
            type="c0mpute.provider.advert/v1"
            href={`${DOCS}/providers.md`}
            body="A provider says what it can run, at what indicative rates, under which trust tier — and until when. Adverts expire within 15 minutes, so a provider that loses power stops looking like live capacity."
          />
          <Msg
            type="c0mpute.job/v2"
            href={`${DOCS}/jobs.md`}
            body="A buyer describes work, requirements, deadline, price cap, trust tier, validation policy and settlement rail. It names no provider, no scheduler and no host. A job's id is the content hash of its own manifest."
          />
          <Msg
            type="c0mpute.offer/v1"
            href={`${DOCS}/offers.md`}
            body="A provider quotes a binding price for one specific job, referencing the advert that backs it. Signed by the provider, selected by the buyer — nothing in between decides."
          />
          <Msg
            type="c0mpute.receipt/v1"
            href={`${DOCS}/receipts.md`}
            body="The provider signs what happened: input, output and runtime by hash, what validation concluded, what was paid. The buyer countersigns separately, or disputes with a stated reason."
          />
        </div>
        <p className="text-[var(--color-dim)]">
          They chain by content hash, so no participant has to be trusted to
          report the previous step honestly. An offer names the job it bids on
          and the advert backing it; a receipt names the job and the offer
          that priced it. A third party holding the JSON can confirm the
          amount charged is the amount quoted.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ buyer-side scheduling ]</h2>
        <p>
          There is no globally authoritative matcher. The decision is made by
          one of: your local client, a managed gateway you explicitly chose,
          or your organization&apos;s own scheduler on a private network.
        </p>
        <CodeBlock>{`$ c0mpute run job.json --policy cheapest
$ c0mpute run job.json --policy balanced   # price + reputation + history
$ c0mpute run job.json --policy trusted
$ c0mpute run job.json --gateway none      # prove it works without us`}</CodeBlock>
        <p>
          Three constraints are checks rather than preferences: the offer must
          be for this job, within the price cap, and able to finish before the
          deadline. Price is compared by value, never as a string — lexically{" "}
          <code>&quot;0.042&quot;</code> sorts above{" "}
          <code>&quot;0.10&quot;</code>. Everything else — reputation,
          latency, provider diversity, jurisdiction — is policy you choose.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ validation ]</h2>
        <p>
          Not all computation can be cheaply or perfectly verified, and
          pretending otherwise would price cheap work out of the market. A job
          picks the level its value justifies.
        </p>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row k="L0 requester" v="you accept the output — low-value inference, image generation" />
            <Row k="L1 schema" v="deterministic or schema checks you can run alone — transcoding, OCR, file transforms" />
            <Row k="L2 spotcheck" v="a sampled fraction re-runs on a second, independent provider" />
            <Row k="L3 quorum" v="run on N providers and compare; needs at least 3 to break a tie" />
            <Row k="L4 attested" v="hardware or runtime attestation — private and enterprise work" />
            <Row k="L5 proof" v="workload-specific proof schemes, where the economics justify them" />
          </tbody>
        </table>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ settlement adapters ]</h2>
        <p>
          The job names its settlement rail on the wire rather than assuming
          one. Adopting c0mpute does not mean adopting one payment product.
        </p>
        <CodeBlock>{`"economics": {
  "maxPrice":   { "amount": "0.10", "currency": "USD" },
  "settlement": "coinpay",
  "escrow":     true
}`}</CodeBlock>
        <p>
          <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">
            CoinPay
          </a>{" "}
          is the default and the best-integrated adapter — escrow, multi-asset
          settlement, signed payment receipts. It is a first-party
          integration, not a requirement: the field is an open string, so{" "}
          <code>x402</code>, <code>lightning</code>, <code>invoice</code>,
          gateway credits or a private organization rail all travel correctly.
          The network keeps working when CoinPay does not.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ indexers and gateways ]</h2>
        <p>
          An indexer may cache adverts and serve fast queries. Because every
          record it serves is independently signed, it can be stale, partial,
          or lying about <em>which</em> records exist — and cannot forge one.
          Two indexers disagreeing is a normal condition, not an incident; a
          buyer who doubts both queries the network directly.
        </p>
        <p>
          A managed gateway submits jobs, collects offers and reads receipts
          through the same messages any node uses. There is no privileged
          internal API and no message type only our infrastructure can
          produce, which is the difference between a protocol and a product
          with an SDK.
        </p>
        <p className="text-[var(--color-dim)]">
          The <Link href="/status">network status page</Link> is one such
          observed view — useful, and not canonical state.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what is built ]</h2>
        <p className="text-[var(--color-dim)]">
          We would rather say this plainly than let a spec page imply working
          code.
        </p>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row k="shipped" v="canonical serialization, native identity, signed envelopes, the four message types, cross-implementation test vectors, provider directory, offer book, buyer-side selection policies, receipt-derived reputation" />
            <Row k="in progress" v="the transport: local daemon, DHT discovery, gossip job announcements, offer collection over the wire, relay paths for providers behind NAT" />
            <Row k="planned" v="settlement adapters, generic container workloads, private network overlays" />
          </tbody>
        </table>
        <p>
          The claim that you do not need us is only worth what it is tested
          against, so <code>--gateway none</code> — a job discovered, awarded,
          executed, validated and receipted with every c0mpute.com service
          unreachable — becomes a release-gating test rather than a
          statement on a webpage.
        </p>
      </section>

      <section className="space-y-2 rule pt-8 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ specification ]</h2>
        <ul className="space-y-1 text-xs">
          <li>→ <a href={`${DOCS}/README.md`}>docs/protocol</a> — index and the message chain</li>
          <li>→ <a href={`${DOCS}/canonical-json.md`}>canonical-json</a> — the exact bytes a signature covers</li>
          <li>→ <a href={`${DOCS}/identity.md`}>identity</a> — <code>did:c0mpute</code>, and why identity is not payment</li>
          <li>→ <a href={`${DOCS}/envelopes.md`}>envelopes</a> — signing and domain separation</li>
          <li>→ <a href={`${REPO}/blob/master/dips/v2.x/0024-protocol-vs-hosted-services.md`}>DIP-0024</a> — protocol vs optional hosted services</li>
          <li>→ <a href={`${REPO}/blob/master/dips/v2.x/0025-signed-envelopes-and-native-identity.md`}>DIP-0025</a> — envelopes and native identity</li>
        </ul>
        <p className="text-xs text-[var(--color-dim)] pt-2">
          The reference implementation is{" "}
          <a href={`${REPO}/tree/master/node/crates/c0mpute-envelope`}>
            <code>c0mpute-envelope</code>
          </a>
          . It depends on no transport, no HTTP client and no settlement
          product — which is what makes &quot;core protocol crates must not
          depend on hosted-service SDKs&quot; a rule CI can check rather than
          a slogan.
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

function Msg({ type, body, href }: { type: string; body: string; href: string }) {
  return (
    <div className="border border-[var(--color-rule)] bg-[var(--color-card)] rounded p-4 space-y-1">
      <p className="text-xs accent">
        <a href={href} className="!border-0 hover:underline">
          {type}
        </a>
      </p>
      <p className="text-xs text-[var(--color-dim)] leading-6">{body}</p>
    </div>
  );
}
