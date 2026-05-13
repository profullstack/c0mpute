"use client";

export default function StatusError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  return (
    <main className="max-w-3xl mx-auto px-6 py-16">
      <div className="rounded-2xl border border-red-400/30 bg-red-400/5 p-8">
        <p className="text-xs font-semibold uppercase tracking-[0.3em] text-red-300">
          network status
        </p>
        <h1 className="mt-2 text-2xl font-bold text-[var(--color-fg)]">
          Could not load network snapshot
        </h1>
        <p className="mt-3 text-sm text-[var(--color-dim)]">
          {error?.message ?? "An unknown error occurred."}
        </p>
        <button
          type="button"
          onClick={() => reset()}
          className="mt-6 rounded-full border border-[var(--color-accent)] px-4 py-2 text-sm text-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/20 transition-colors"
        >
          Try again
        </button>
      </div>
    </main>
  );
}
