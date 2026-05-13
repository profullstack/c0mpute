export interface ResourceColumn {
  key: string;
  label: string;
}

function formatValue(value: unknown) {
  if (value === null || value === undefined || value === "") {
    return "\u2014";
  }
  return String(value);
}

export default function ResourceTable({
  title,
  description,
  columns,
  rows,
  emptyMessage,
}: {
  title: string;
  description?: string;
  columns: ResourceColumn[];
  rows: Record<string, unknown>[];
  emptyMessage: string;
}) {
  return (
    <section className="overflow-hidden rounded-2xl border border-[var(--color-rule)] bg-[var(--color-card)]">
      <div className="border-b border-[var(--color-rule)] px-5 py-4">
        <h2 className="text-base font-semibold text-[var(--color-accent)]">
          {title}
        </h2>
        {description && (
          <p className="mt-1 text-xs text-[var(--color-muted)]">
            {description}
          </p>
        )}
      </div>
      <div className="overflow-x-auto">
        <table className="min-w-full text-left text-sm">
          <thead className="text-[var(--color-dim)]">
            <tr>
              {columns.map((column) => (
                <th
                  key={column.key}
                  className="px-5 py-3 text-xs font-medium uppercase tracking-[0.15em]"
                >
                  {column.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td
                  className="px-5 py-8 text-[var(--color-muted)] text-xs"
                  colSpan={columns.length}
                >
                  {emptyMessage}
                </td>
              </tr>
            ) : (
              rows.map((row, index) => (
                <tr
                  key={
                    typeof row.id === "string" || typeof row.id === "number"
                      ? String(row.id)
                      : `${title}-${index}`
                  }
                  className="border-t border-[var(--color-line)]"
                >
                  {columns.map((column) => (
                    <td
                      key={column.key}
                      className="px-5 py-3.5 text-[var(--color-fg)] font-mono text-xs"
                    >
                      {formatValue(row[column.key])}
                    </td>
                  ))}
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}
