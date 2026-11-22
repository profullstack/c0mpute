"use client";

// A small, safe Markdown renderer for chat answers: React elements only (no
// innerHTML), links limited to http(s). Covers what models write: headings,
// paragraphs, **bold**, *italic*, `code`, fenced code, lists, quotes, rules,
// tables and links, plus [n] citations that jump to the turn's source cards.

import type { ReactNode } from "react";

export type CiteTarget = { n: number; url: string; title: string };

const SAFE_URL = /^https?:\/\/[^\s<>"]+$/i;

function inline(text: string, cites: CiteTarget[] | undefined, key: string): ReactNode[] {
  const out: ReactNode[] = [];
  // Order matters: code first (nothing inside it is formatted), then links, citations, bold, italic, bare URLs.
  const re = /(`[^`\n]+`)|(\[([^\]\n]{1,200})\]\((https?:\/\/[^\s)]+)\))|(\[(\d{1,2})\](?!\())|(\*\*([^*\n]+)\*\*)|(__([^_\n]+)__)|(\*([^*\n]+)\*)|(\b_([^_\n]+)_\b)|(https?:\/\/[^\s<>()"]+[^\s<>()".,;:!?])/g;
  let last = 0;
  let i = 0;
  for (let m: RegExpExecArray | null; (m = re.exec(text)); ) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const k = `${key}-${i++}`;
    if (m[1]) out.push(<code key={k} className="px-1 bg-[var(--color-rule)]/40">{m[1].slice(1, -1)}</code>);
    else if (m[2]) {
      const href = m[4]!;
      out.push(
        SAFE_URL.test(href) ? (
          <a key={k} href={href} target="_blank" rel="noopener noreferrer nofollow" className="underline accent">
            {m[3]}
          </a>
        ) : (
          m[3]
        ),
      );
    } else if (m[5]) {
      const n = Number(m[6]);
      const c = cites?.find((s) => s.n === n);
      out.push(
        c ? (
          <a key={k} href={c.url} target="_blank" rel="noopener noreferrer nofollow" title={c.title} className="accent no-underline text-[0.75em] align-super">
            [{n}]
          </a>
        ) : (
          m[5]
        ),
      );
    } else if (m[7]) out.push(<strong key={k}>{inline(m[8]!, cites, k)}</strong>);
    else if (m[9]) out.push(<strong key={k}>{inline(m[10]!, cites, k)}</strong>);
    else if (m[11]) out.push(<em key={k}>{inline(m[12]!, cites, k)}</em>);
    else if (m[13]) out.push(<em key={k}>{inline(m[14]!, cites, k)}</em>);
    else if (m[15]) {
      const href = m[15];
      out.push(
        <a key={k} href={href} target="_blank" rel="noopener noreferrer nofollow" className="underline accent break-all">
          {href}
        </a>,
      );
    }
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

const cells = (line: string) =>
  line
    .trim()
    .replace(/^\||\|$/g, "")
    .split("|")
    .map((c) => c.trim());

export function Markdown({ text, cites }: { text: string; cites?: CiteTarget[] }) {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const blocks: ReactNode[] = [];
  let i = 0;
  let b = 0;
  const para: string[] = [];
  const flush = () => {
    if (!para.length) return;
    const k = `p${b++}`;
    blocks.push(
      <p key={k} className="my-2">
        {para.flatMap((l, j) => (j ? [<br key={`${k}-br${j}`} />, ...inline(l, cites, `${k}-${j}`)] : inline(l, cites, `${k}-${j}`)))}
      </p>,
    );
    para.length = 0;
  };
  while (i < lines.length) {
    const line = lines[i]!;
    const t = line.trim();
    if (t.startsWith("```")) {
      flush();
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i]!.trim().startsWith("```")) body.push(lines[i++]!);
      i++;
      blocks.push(
        <pre key={`c${b++}`} className="my-2 p-3 overflow-x-auto border border-[var(--color-rule)] text-[0.85em]">
          <code>{body.join("\n")}</code>
        </pre>,
      );
      continue;
    }
    const h = /^(#{1,6})\s+(.*)$/.exec(t);
    if (h) {
      flush();
      const level = h[1]!.length;
      const k = `h${b++}`;
      const cls = level <= 2 ? "mt-4 mb-1 font-bold text-[1.05em]" : "mt-3 mb-1 font-bold";
      blocks.push(
        <div key={k} role="heading" aria-level={Math.min(6, level + 1)} className={cls}>
          {inline(h[2]!, cites, k)}
        </div>,
      );
      i++;
      continue;
    }
    if (/^(-{3,}|\*{3,}|_{3,})$/.test(t)) {
      flush();
      blocks.push(<hr key={`r${b++}`} className="my-3 border-[var(--color-rule)]" />);
      i++;
      continue;
    }
    if (t.startsWith("|") && i + 1 < lines.length && /^\|?\s*:?-{2,}/.test(lines[i + 1]!.trim())) {
      flush();
      const head = cells(t);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && lines[i]!.trim().startsWith("|")) rows.push(cells(lines[i++]!));
      const k = `t${b++}`;
      blocks.push(
        <div key={k} className="my-2 overflow-x-auto">
          <table className="text-[0.9em] border-collapse">
            <thead>
              <tr>
                {head.map((c, j) => (
                  <th key={j} className="border border-[var(--color-rule)] px-2 py-1 text-left font-bold">
                    {inline(c, cites, `${k}-h${j}`)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((r, ri) => (
                <tr key={ri}>
                  {head.map((_, j) => (
                    <td key={j} className="border border-[var(--color-rule)] px-2 py-1 align-top">
                      {inline(r[j] ?? "", cites, `${k}-${ri}-${j}`)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>,
      );
      continue;
    }
    const li = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/.exec(line);
    if (li) {
      flush();
      const ordered = /\d/.test(li[2]!);
      const items: string[] = [];
      while (i < lines.length) {
        const m = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/.exec(lines[i]!);
        if (m) items.push(m[3]!);
        else if (lines[i]!.trim() && /^\s{2,}/.test(lines[i]!) && items.length) items[items.length - 1] += ` ${lines[i]!.trim()}`;
        else break;
        i++;
      }
      const k = `l${b++}`;
      const kids = items.map((it, j) => (
        <li key={j} className="my-0.5">
          {inline(it, cites, `${k}-${j}`)}
        </li>
      ));
      blocks.push(
        ordered ? (
          <ol key={k} className="my-2 pl-6 list-decimal">
            {kids}
          </ol>
        ) : (
          <ul key={k} className="my-2 pl-6 list-disc">
            {kids}
          </ul>
        ),
      );
      continue;
    }
    if (t.startsWith(">")) {
      flush();
      const q: string[] = [];
      while (i < lines.length && lines[i]!.trim().startsWith(">")) q.push(lines[i++]!.trim().replace(/^>\s?/, ""));
      const k = `q${b++}`;
      blocks.push(
        <blockquote key={k} className="my-2 pl-3 border-l-2 border-[var(--color-rule)] text-[var(--color-dim)]">
          {inline(q.join(" "), cites, k)}
        </blockquote>,
      );
      continue;
    }
    if (!t) {
      flush();
      i++;
      continue;
    }
    para.push(line);
    i++;
  }
  flush();
  return <div className="break-words">{blocks}</div>;
}

export type SourceCard = { n: number; title: string; url: string; host: string; image: string | null; snippet: string };

/** The web pages a grounded answer drew on: a photo strip with captions, then the rest as links. */
export function SourceCards({ sources }: { sources: SourceCard[] }) {
  if (!sources.length) return null;
  const withImage = sources.filter((s) => s.image);
  return (
    <div className="mt-2 mb-1 text-xs">
      {withImage.length > 0 && (
        <div className="flex gap-2 overflow-x-auto pb-1">
          {withImage.map((s) => (
            <a
              key={s.n}
              href={s.url}
              target="_blank"
              rel="noopener noreferrer nofollow"
              className="shrink-0 w-48 border border-[var(--color-rule)] hover:border-[var(--color-fg)] no-underline"
            >
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img
                src={s.image!}
                alt=""
                loading="lazy"
                referrerPolicy="no-referrer"
                className="w-full h-28 object-cover bg-[var(--color-rule)]/30"
                onError={(e) => {
                  (e.currentTarget as HTMLImageElement).style.display = "none";
                }}
              />
              <span className="block p-1.5 leading-snug">
                <span className="accent">[{s.n}]</span> <span className="text-[var(--color-fg)]">{s.title}</span>
                <span className="block text-[var(--color-dim)]">{s.host}</span>
              </span>
            </a>
          ))}
        </div>
      )}
      <ol className="mt-1 space-y-0.5 text-[var(--color-dim)]">
        {sources.map((s) => (
          <li key={s.n} className="truncate">
            <span className="accent">[{s.n}]</span>{" "}
            <a href={s.url} target="_blank" rel="noopener noreferrer nofollow" className="underline hover:text-[var(--color-fg)]">
              {s.title}
            </a>{" "}
            · {s.host}
          </li>
        ))}
      </ol>
    </div>
  );
}
