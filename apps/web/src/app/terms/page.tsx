export const metadata = { title: "terms — c0mpute" };

export default function TermsPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8 text-sm leading-7">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">terms of service</h1>
        <p className="comment">// last updated 2026-11-10 — v1 placeholder</p>
      </header>

      <Block title="agreement">
        <p>
          By installing or using the c0mpute software, the c0mpute network,
          or any of its modules (collectively, the &quot;Service&quot;), you
          agree to these terms.
        </p>
      </Block>

      <Block title="open source">
        <p>
          The c0mpute, coinpay, and infernet binaries are released under the
          dual <code>MIT</code> license. Your rights and
          obligations under those licenses apply to the code itself.
        </p>
      </Block>

      <Block title="acceptable use">
        <p>
          The c0mpute network is general-purpose compute infrastructure.
          You may not use it to:
        </p>
        <ul className="list-disc pl-6 space-y-1">
          <li>Process content that is illegal in your jurisdiction or where workers run.</li>
          <li>Distribute CSAM, malware, or content that infringes third-party copyright.</li>
          <li>Generate sexual content involving minors or real, identifiable people (see below).</li>
          <li>Impersonate other DIDs or attempt to forge signed receipts.</li>
          <li>Submit jobs designed to compromise worker hosts.</li>
        </ul>
      </Block>

      <Block title="generated images and video">
        <p>
          Image and video generation is safe-for-work by default: explicit
          requests are refused and generated results are checked before they
          are delivered.
        </p>
        <p>
          <strong>Adult content</strong> is allowed only in adult mode, which is
          for verified adults: you must attest that you are 18 or older (and
          that adult content is legal where you are) in the chat settings, and
          have a paid compute balance. Anonymous and free-tier use never
          includes adult mode. Every node on the network serves adult jobs. Adult results are shown blurred until you open them and are
          never used for previews, public galleries, shares or published sites.
          Adult mode is text only: no uploaded images, photos or face references.
        </p>
        <p>Never allowed, in any mode, on any node:</p>
        <ul className="list-disc pl-6 space-y-1">
          <li>
            Child sexual abuse material, or any sexual or sexualized depiction of
            minors, including fictional, drawn, animated or &quot;young-looking&quot;
            characters, and any request that combines sexual content with
            indicators of a minor.
          </li>
          <li>
            Sexual or intimate content of a real, identifiable person
            (deepfakes, &quot;nudify&quot; requests, non-consensual intimate
            imagery), including celebrities, public figures, exes, partners,
            coworkers or anyone else.
          </li>
        </ul>
        <p>
          Requests are screened automatically on c0mpute.com, on every gateway
          and again on every worker. Detection of real people is a heuristic and
          does not make such requests acceptable when it misses them. Breaking
          these rules leads to termination of the account, and suspected child
          sexual abuse material and non-consensual intimate imagery are reported
          to the relevant authorities (including NCMEC) as the law requires.
        </p>
      </Block>

      <Block title="payments">
        <p>
          Payment flows run through CoinPay escrow. Disputes are handled per
          the CoinPay terms; <code>c0mpute.com</code> is the marketplace, not
          the regulated payment entity.
        </p>
      </Block>

      <Block title="no warranty">
        <p>
          The Service is provided &quot;as is.&quot; The project is permissive
          open source; you run it at your own risk.
        </p>
      </Block>

      <Block title="changes">
        <p>
          These terms will be revised before public launch and on a continuing
          basis. Material changes will be announced via the GitHub repo and
          the c0mpute.com release feed.
        </p>
      </Block>

      <p className="text-xs text-[var(--color-dim)] rule pt-6">
        This page is a placeholder. A real ToS reviewed by counsel ships
        before public mainnet launch.
      </p>
    </div>
  );
}

function Block({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-2">
      <h2 className="text-base accent">[ {title} ]</h2>
      <div className="pl-5 space-y-2">{children}</div>
    </section>
  );
}
