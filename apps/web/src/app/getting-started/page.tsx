import { CodeBlock } from "@/components/CodeBlock";

export const metadata = {
  title: "getting-started — c0mpute",
  description: "Get up and running with c0mpute in minutes. Install the CLI, create your CoinPay DID, register as a worker, and submit your first GPU compute job.",
  alternates: { canonical: "https://c0mpute.com/getting-started" },
};

export default function GettingStartedPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">getting-started</h1>
        <p className="comment">// install → identity → register → run</p>
      </header>

      <Section number="1" label="install the cli stack">
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Installs three binaries into <code>~/.c0mpute/bin</code>:{" "}
          <span className="accent">c0mpute</span>,{" "}
          <span className="accent">coinpay</span>,{" "}
          <span className="accent">infernet</span>. No sudo. The worker
          self-upgrades every 60 seconds.
        </p>
      </Section>

      <Section number="2" label="sign in everywhere">
        <CodeBlock>{`$ c0mpute login`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Signs in to coinpay (payments + your payable DID) and infernet (ties
          this node to your infernetprotocol.com account), one at a time.{" "}
          <code>c0mpute worker register</code> then sets up the DID
          automatically (uses your existing one or creates it).
        </p>
      </Section>

      <Section number="3" label="run a worker">
        <CodeBlock>{`$ c0mpute worker register
$ c0mpute worker start --gpu`}</CodeBlock>
      </Section>

      <Section number="4" label="submit a job">
        <CodeBlock>{`$ c0mpute transcode submit input.mov --preset hls --max-price 1.25
$ c0mpute infernet chat --model qwen2.5:7b "hello"
$ c0mpute job status <job-id>`}</CodeBlock>
      </Section>

      <Section number="5" label="interactive tui">
        <CodeBlock>{`$ c0mpute tui`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Live worker / job dashboard, terminal-native (react-blessed).
        </p>
      </Section>

      <Section number="6" label="upgrade & uninstall">
        <CodeBlock>{`$ c0mpute update              # check for + apply new release
$ c0mpute upgrade --check     # alias; just check, don't apply
$ c0mpute uninstall           # remove c0mpute
$ c0mpute uninstall --all     # remove c0mpute, coinpay, infernet
$ c0mpute uninstall --purge   # also remove ~/.config/c0mpute`}</CodeBlock>
      </Section>

      <Section number="7" label="check the stack">
        <CodeBlock>{`$ c0mpute doctor`}</CodeBlock>
      </Section>

      <Section number="8" label="host a site on c0mpute and point a Moshpit name at it">
        <p className="text-sm text-[var(--color-dim)]">
          Publish a directory of static files once. Hosting nodes keep three
          copies, replace any copy whose node disappears, and every c0mpute
          gateway serves it. Live example:{" "}
          <a className="accent" href="http://alt.2600/">alt.2600</a> is site{" "}
          <code>ea0e1f5afde44477e71d</code>, also at{" "}
          <a className="accent" href="https://gateway.c0mpute.com/s/ea0e1f5afde44477e71d/">
            gateway.c0mpute.com/s/ea0e1f5afde44477e71d/
          </a>{" "}
          and, for browsers without Moshpit DNS,{" "}
          <a className="accent" href="https://alt.2600.pit.world/">alt.2600.pit.world</a>.
        </p>

        <p className="text-sm">publish (after step 1):</p>
        <CodeBlock>{`$ c0mpute hosting publish ./public --name mysite
site:    ea0e1f5afde44477e71d
version: 1
copies:  3/3
path:    /s/ea0e1f5afde44477e71d/   (on any c0mpute gateway, e.g. http://<gateway>:7777/s/ea0e1f5afde44477e71d/)

$ c0mpute hosting ls          # sites a gateway knows, with live copy counts`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          The site is signed with this machine&apos;s key, so only this machine
          can publish new versions: run the same command with the same{" "}
          <code>--name</code> to update it. Any gateway serves it at{" "}
          <code>/s/&lt;site_id&gt;/</code>, for example{" "}
          <code>https://gateway.c0mpute.com/s/&lt;site_id&gt;/</code>, with
          byte ranges so video seeks. Limits: 64 GiB per free site, 256 GiB
          with <code>--disks auto</code>. Sites over 1 GiB are striped over the
          network in 64 MiB groups; run the same command again to resume an
          interrupted publish.
        </p>

        <p className="text-sm">point a Moshpit name at it (free to register):</p>
        <CodeBlock>{`$ moshcode name c0mpute alt.2600 ea0e1f5afde44477e71d
$ moshcode dns resolve alt.2600
alt.2600 → edge.c0mpute.com

$ moshcode name c0mpute alt.2600 off  # stop serving it from c0mpute`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          The registry now answers the name with the c0mpute edge
          (edge.c0mpute.com). The edge holds a Moshpit CA certificate for the
          name and fetches <code>/s/&lt;site_id&gt;/</code> from whichever
          gateway is healthy, so <code>https://alt.2600</code> works for anyone
          on Moshpit DNS (<code>moshcode dns enable</code> installs the root)
          and <code>http://alt.2600</code> works without the root. Pointing
          the name at a server again takes it off c0mpute.
        </p>

        <p className="text-sm">or a domain you own:</p>
        <CodeBlock>{`$ c0mpute hosting publish ./public --name mysite --domain blog.example.org
# DNS:
#   TXT  _c0mpute-hosting.blog.example.org  "site=<site_id>"
#   A/AAAA  blog.example.org  ->  one or more gateways (TLS in front of :7777)`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          A gateway serves the domain only when both agree: the signed site
          record lists it and its TXT names the site.
        </p>

        <p className="text-sm">pay hosting nodes to keep it (d1sks marketplace):</p>
        <CodeBlock>{`$ c0mpute hosting disks --country DE,FI     # what --disks auto would pick
$ c0mpute hosting publish ./public --name mysite --disks auto
$ c0mpute hosting publish ./public --name mysite --disks auto --max-price 0.01 --copies 3`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Picks preferred hosts from{" "}
          <a className="accent" href="https://d1sks.pwa.pit.world/disks?hosting=1">d1sks</a>,
          cheapest first. Keep a node running on the publishing machine: it
          challenges the holders and settles, as a dry run until{" "}
          <code>[payments]</code> is enabled. Free copies stay free.
        </p>

        <p className="text-sm">run your own public gateway:</p>
        <CodeBlock>{`# ~/.config/c0mpute/config.toml  (gateway and hosting are default roles)
[gateway]
bind = "0.0.0.0:7777"
announce = "https://gw.example.org"   # TLS in front of :7777`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          An announced gateway is listed as <code>gateway</code> on{" "}
          <code>/api/nodes</code>, where the Moshpit edge and other clients find
          gateways to fetch sites from. More in{" "}
          <a className="accent" href="https://github.com/profullstack/c0mpute/blob/master/docs/hosting.md">
            docs/hosting.md
          </a>
          .
        </p>
      </Section>
    </div>
  );
}

function Section({
  number,
  label,
  children,
}: {
  number: string;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-3">
      <h2 className="text-lg">
        <span className="accent">[{number}]</span>{" "}
        <span className="text-[var(--color-fg)]">{label}</span>
      </h2>
      <div className="space-y-2 pl-5">{children}</div>
    </section>
  );
}
