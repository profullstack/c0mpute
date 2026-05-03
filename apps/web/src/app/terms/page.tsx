export const metadata = { title: "terms — c0mpute" };

export default function TermsPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8 text-sm leading-7">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">terms of service</h1>
        <p className="comment">// last updated 2026-05-03 — v1 placeholder</p>
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
          <li>Impersonate other DIDs or attempt to forge signed receipts.</li>
          <li>Submit jobs designed to compromise worker hosts.</li>
        </ul>
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
