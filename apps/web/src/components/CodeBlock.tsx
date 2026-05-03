"use client";

import { useState } from "react";

/**
 * Terminal-style code block with a copy button. Used everywhere on the
 * site that shows a shell command. Strips a leading "$ " from the
 * displayed-but-not-copied prompt so users get the bare command.
 */
export function CodeBlock({
  children,
  className = "",
}: {
  children: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    // Strip leading "$ " from each line if present (the visual prompt)
    // so the copied command is runnable as-is.
    const text = children
      .split("\n")
      .map((l) => l.replace(/^\s*\$\s/, ""))
      .join("\n")
      .trim();
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      // Older browsers / non-secure contexts: fall through silently.
    }
  }

  return (
    <div
      className={`relative group bg-[var(--color-card)] border border-[var(--color-rule)] rounded ${className}`}
    >
      <pre className="p-4 overflow-x-auto text-xs leading-5 whitespace-pre">
        <code>{children}</code>
      </pre>
      <button
        type="button"
        onClick={copy}
        aria-label="copy"
        className="absolute top-2 right-2 px-2 py-1 text-[10px] uppercase tracking-wider text-[var(--color-dim)] hover:text-[var(--color-accent)] border border-[var(--color-rule)] rounded bg-[var(--color-bg)] opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity !border-[var(--color-rule)] hover:!border-[var(--color-accent)]"
      >
        {copied ? "✓ copied" : "copy"}
      </button>
    </div>
  );
}
