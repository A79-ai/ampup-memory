/**
 * Markdown as memstore writes it (a compiled skill, a tool's answer, advice): headings, paragraphs, numbered and
 * bulleted lists, pipe tables, fenced code, and inline **bold**, `code` and [links](https://…). Drawn as elements, never as
 * HTML: the text is data. `code` may turn an inline code span into a link (a skill's name in its goal's index).
 */
export function Markdown({
  text,
  code,
  cite,
}: {
  text: string;
  code?: (span: string) => React.ReactNode | null;
  /** How a citation (`[e6 @347]`, `[topic:memstore_cli]`) reads, given what's inside its brackets; as written where it gives none. */
  cite?: (inside: string) => React.ReactNode | null;
}) {
  const inline = (s: string, at: number): React.ReactNode[] =>
    s
      .split(
        /(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\(https?:\/\/[^)\s]+\)|\[(?:e\d+|[a-z][a-z0-9_]*:[^\s\]])[^\]\n]*\](?!\())/,
      )
      .map((part, i) => {
        const k = `${at}-${i}`;
        if (cite && /^\[(?:e\d+|[a-z][a-z0-9_]*:[^\s\]])[^\]\n]*\]$/.test(part)) {
          const shown = cite(part.slice(1, -1));
          if (shown != null) return <span key={k}>{shown}</span>;
        }
        if (part.startsWith("**") && part.endsWith("**") && part.length > 4)
          return <b key={k}>{part.slice(2, -2)}</b>;
        if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
          const span = part.slice(1, -1);
          return <span key={k}>{code?.(span) ?? <code>{span}</code>}</span>;
        }
        const link = /^\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)$/.exec(part);
        if (link)
          return (
            <a key={k} href={link[2]} target="_blank" rel="noopener noreferrer">
              {link[1]}
            </a>
          );
        return part;
      });
  const out: React.ReactNode[] = [];
  let list: { ordered: boolean; start: number; items: React.ReactNode[] } | null = null;
  let table: string[][] | null = null;
  let fence: string[] | null = null;
  const flush = () => {
    if (!list) return;
    const items = list.items.map((it, i) => <li key={i}>{it}</li>);
    out.push(
      list.ordered ? (
        <ol key={out.length} start={list.start}>
          {items}
        </ol>
      ) : (
        <ul key={out.length}>{items}</ul>
      ),
    );
    list = null;
  };
  const flushTable = (n: number) => {
    if (!table) return;
    const [head, ...rows] = table;
    out.push(
      <table key={out.length} className="md-table">
        <thead>
          <tr>
            {(head ?? []).map((c, i) => (
              <th key={i}>{inline(c, n)}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              {r.map((c, j) => (
                <td key={j}>{inline(c, n * 1000 + i)}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>,
    );
    table = null;
  };
  text.split("\n").forEach((line, n) => {
    if (/^\s*```/.test(line)) {
      if (fence) {
        out.push(
          <pre key={out.length} className="code">
            {fence.join("\n")}
          </pre>,
        );
        fence = null;
      } else {
        flush();
        fence = [];
      }
      return;
    }
    if (fence) {
      fence.push(line);
      return;
    }
    // A pipe table: its rows, the separator row (|---|---|) left out.
    if (/^\s*\|.*\|\s*$/.test(line)) {
      flush();
      if (!/^\s*\|[\s:|-]+\|\s*$/.test(line))
        (table ??= []).push(
          line
            .trim()
            .slice(1, -1)
            .split("|")
            .map((c) => c.trim()),
        );
      return;
    }
    flushTable(n);
    const h = /^(#{1,4}) (.*)$/.exec(line);
    const li = /^\s*(\d+\.|[-*]) (.*)$/.exec(line);
    if (li) {
      const ordered = /\d/.test(li[1] ?? "");
      if (list && list.ordered !== ordered) flush();
      // A numbered list resumes at its own number, after a nested bullet list broke it.
      list ??= { ordered, start: ordered ? parseInt(li[1] ?? "1", 10) : 1, items: [] };
      list.items.push(inline(li[2] ?? "", n));
      return;
    }
    flush();
    if (h) {
      const Tag = (h[1] ?? "").length <= 2 ? "h3" : "h4";
      out.push(<Tag key={out.length}>{inline(h[2] ?? "", n)}</Tag>);
    } else if (line.trim()) {
      out.push(<p key={out.length}>{inline(line, n)}</p>);
    }
  });
  flush();
  flushTable(-1);
  if (fence)
    out.push(
      <pre key={out.length} className="code">
        {(fence as string[]).join("\n")}
      </pre>,
    );
  return <div className="md">{out}</div>;
}
