export interface OverviewCard {
  label: string;
  value: string | number;
  note: string;
}

export default function OverviewGrid({ cards }: { cards: OverviewCard[] }) {
  return (
    <section className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
      {cards.map((card) => (
        <article
          key={card.label}
          className="rounded-2xl border border-[var(--color-rule)] bg-[var(--color-card)] p-5"
        >
          <p className="text-xs uppercase tracking-[0.2em] text-[var(--color-dim)]">
            {card.label}
          </p>
          <div className="mt-4 flex items-end justify-between gap-3">
            <p className="text-3xl font-bold text-[var(--color-accent)] tracking-tight tabular-nums">
              {card.value}
            </p>
            <span className="rounded-full bg-[var(--color-accent-dim)]/15 px-2.5 py-1 text-[11px] text-[var(--color-accent)]">
              {card.note}
            </span>
          </div>
        </article>
      ))}
    </section>
  );
}
