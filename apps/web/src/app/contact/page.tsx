export const metadata = {
  title: "contact — c0mpute",
  description: "Reach the c0mpute project: GitHub Issues for bugs and features, hello@c0mpute.com for general inquiries, security@c0mpute.com for vulnerability disclosure.",
  alternates: { canonical: "https://c0mpute.com/contact" },
};

export default function ContactPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">contact</h1>
        <p className="comment">// reach the project</p>
      </header>

      <ul className="space-y-3 text-sm leading-6">
        <li>
          <span className="accent">github</span>{" "}
          <a href="https://github.com/profullstack/c0mpute/issues">
            github.com/profullstack/c0mpute/issues
          </a>{" "}
          <span className="text-[var(--color-dim)]">— bugs, features, discussions</span>
        </li>
        <li>
          <span className="accent">email</span>{" "}
          <a href="mailto:hello@c0mpute.com">hello@c0mpute.com</a>{" "}
          <span className="text-[var(--color-dim)]">— anything else</span>
        </li>
        <li>
          <span className="accent">abuse</span>{" "}
          <a href="mailto:abuse@c0mpute.com">abuse@c0mpute.com</a>{" "}
          <span className="text-[var(--color-dim)]">— DMCA / illegal content reports</span>
        </li>
        <li>
          <span className="accent">security</span>{" "}
          <a href="mailto:security@c0mpute.com">security@c0mpute.com</a>{" "}
          <span className="text-[var(--color-dim)]">— vulnerability disclosure</span>
        </li>
      </ul>

      <p className="text-xs text-[var(--color-dim)] rule pt-6">
        Contact addresses are placeholders until the project's mail / issue
        triage is wired up.
      </p>
    </div>
  );
}
