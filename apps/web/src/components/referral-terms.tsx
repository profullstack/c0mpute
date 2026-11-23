// The public referral terms (nodes and people), from Infernet's live config
// (the same numbers its ledger uses). `terms.term` is the reader's own node
// commission term.
type Terms = {
  commission_pct_of_house_fee: number;
  house_fee_pct: number | null;
  effective_pct_of_node_revenue: number | null;
  spend?: { enabled: boolean; commission_pct_of_house_fee: number; effective_pct_of_spend: number | null; term_months: number | null } | null;
  term: { arm: string; label: string | null; months: number | null } | null;
  min_payout_usd: number;
  auto_approve_max_usd: number;
};

const usd = (n: number) => `$${(Number(n) || 0).toFixed(2).replace(/\.00$/, "")}`;

export function ReferralTerms({ terms }: { terms: Terms | null }) {
  const pct = terms?.commission_pct_of_house_fee ?? 5;
  return (
    <section id="terms" className="space-y-3 text-sm leading-7">
      <h2 className="text-lg font-bold accent">referral terms</h2>
      <ul className="list-disc pl-5 space-y-2 text-[var(--color-dim)]">
        <li>
          You earn <span className="accent">{pct}% of the house fee</span> on each paid job served by a node you referred.
          {terms?.house_fee_pct != null
            ? ` With today's ${terms.house_fee_pct}% house fee that is about ${terms.effective_pct_of_node_revenue}% of what that node's jobs bring in.`
            : ""}
        </li>
        {terms?.spend?.enabled ? (
          <li>
            People you refer: you earn <span className="accent">{terms.spend.commission_pct_of_house_fee}% of the house fee</span> collected
            on what they spend on paid inference and paid jobs (chat, API, images and the rest).
            {terms.house_fee_pct != null && terms.spend.effective_pct_of_spend != null
              ? ` With today's ${terms.house_fee_pct}% house fee that is ${terms.spend.effective_pct_of_spend}% of their spend.`
              : ""}{" "}
            {terms.spend.term_months ? `It runs for ${terms.spend.term_months} months from when they joined.` : "It does not expire."} A
            person counts as referred when they create their account after opening your link (first touch); it cannot be added later,
            and they pay the normal price.
          </li>
        ) : null}
        <li>
          The commission comes out of the house fee. It is never taken from the node operator&apos;s earnings and is never more than
          the house earned on that job. Jobs that earn the house no fee (free tier, subsidised or unpaid jobs) earn no commission.
        </li>
        {terms?.term ? (
          <li>
            Your commission term: {terms.term.label}
            {terms.term.months ? ` (${terms.term.months} months)` : ""}. It was set when your link was created and does not change.
          </li>
        ) : (
          <li>Each referrer&apos;s commission term is set once, when their link is created, and is shown on their referral page.</li>
        )}
        <li>
          A node counts as referred when it registers with a referral code, before its first paid job. Each node has one referrer, and
          that cannot be changed later.
        </li>
        <li>
          No self-referral: nodes you operate, nodes that share your payout wallet or your network, your own accounts, accounts that
          share your payout wallet, and jobs you pay for yourself earn no commission. The house handle (profullstack) and house accounts
          earn nothing. Commissions from self-referral or abuse may be voided.
        </li>
        <li>
          A commission is confirmed when its job&apos;s payment is confirmed. Payouts use the same path as node earnings: minimum{" "}
          {usd(terms?.min_payout_usd ?? 0)}, requests up to {usd(terms?.auto_approve_max_usd ?? 0)} are sent automatically through
          CoinPay, larger ones are reviewed first.
        </li>
        <li>
          Payouts may require tax information. US referrers may receive a Form 1099 once their payments reach the IRS reporting
          threshold, the same as node payouts.
        </li>
        <li>The rate can change for future jobs; a commission already earned keeps the rate it was earned at. Nothing here promises any amount of income.</li>
      </ul>
    </section>
  );
}
