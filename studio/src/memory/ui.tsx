import { useState } from "react";

// The page's small parts: icons, and how loading, empty and failed views look, so every view says the same
// thing the same way.

const PATHS: Record<string, string> = {
  folder:
    "M2.5 5A1.5 1.5 0 0 1 4 3.5h2.6l1.4 1.5h4A1.5 1.5 0 0 1 13.5 6.5v5A1.5 1.5 0 0 1 12 13H4a1.5 1.5 0 0 1-1.5-1.5z",
  entity: "M8 2 13.5 5v6L8 14 2.5 11V5z M2.5 5 8 8l5.5-3 M8 8v6",
  skill: "M9 1.5 3.5 9H8l-1 5.5L12.5 7H8z",
  skill_index: "M3 4h10 M3 7h10 M3 10h10 M3 13h6",
  episode:
    "M4.5 2h4.5l3 3v8.5a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5v-11a.5.5 0 0 1 .5-.5z M9 2v3h3 M6 8.5h4 M6 11h4",
  doc: "M4.5 2h4.5l3 3v8.5a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5v-11a.5.5 0 0 1 .5-.5z M9 2v3h3 M6 8.5h4 M6 11h4",
  chat: "M2.5 4A1.5 1.5 0 0 1 4 2.5h8A1.5 1.5 0 0 1 13.5 4v5.5A1.5 1.5 0 0 1 12 11H7l-3 2.5V11a1.5 1.5 0 0 1-1.5-1.5z",
  call: "M3 3.5c0 5.2 4.3 9.5 9.5 9.5l1-2.5-2.6-1.3-1.2 1.3A7 7 0 0 1 5.5 6.3l1.3-1.2L5.5 2.5z",
  crm: "M2.5 13.5h11 M4 13.5V8 M7 13.5V5 M10 13.5V9.5 M13 13.5V3",
  code: "M5.5 4.5 2 8l3.5 3.5 M10.5 4.5 14 8l-3.5 3.5 M9 3 7 13",
  notes: "M3.5 2.5h9v11h-9z M5.5 5.5h5 M5.5 8h5 M5.5 10.5h3",
  search: "M7 12A5 5 0 1 0 7 2a5 5 0 0 0 0 10z M10.6 10.6 14 14",
  tools:
    "M10 2.5a3 3 0 0 0-2.8 4L2.5 11.2 4.8 13.5l4.7-4.7A3 3 0 0 0 13.5 6l-1.8 1.8-1.7-.3-.3-1.7L11.5 4A3 3 0 0 0 10 2.5z",
  chevron: "M6 4l4 4-4 4",
  down: "M4 6l4 4 4-4",
  sun: "M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z M8 1v1.5 M8 13.5V15 M1 8h1.5 M13.5 8H15 M3 3l1 1 M12 12l1 1 M3 13l1-1 M12 4l1-1",
  moon: "M13.5 9.5A5.5 5.5 0 0 1 6.5 2.5a5.5 5.5 0 1 0 7 7z",
  system: "M2 3.5h12v8H2z M6 14h4 M8 11.5V14",
  menu: "M2.5 4h11 M2.5 8h11 M2.5 12h11",
  close: "M4 4l8 8 M12 4l-8 8",
  alert: "M8 1.8 14.5 13.5h-13z M8 6v3.5 M8 11.5v.5",
  lock: "M4 7h8v6.5H4z M5.5 7V5a2.5 2.5 0 0 1 5 0v2",
  copy: "M5.5 5.5h7v8h-7z M3.5 10.5v-8h7",
  check: "M3 8.5 6.5 12 13 4.5",
  clock: "M8 14A6 6 0 1 0 8 2a6 6 0 0 0 0 12z M8 4.5V8l2.5 1.5",
  quote: "M3 9.5h3v3.5H3z M3 9.5C3 6.5 4 5 6 4.5 M10 9.5h3v3.5h-3z M10 9.5c0-3 1-4.5 3-5",
  branch:
    "M4.5 2.5v11 M4.5 9.5c0-2 1.5-3 4-3h1 M11.5 8a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z M4.5 15a1.5 1.5 0 1 0 0-3",
  database:
    "M2.5 4c0-1 2.5-1.8 5.5-1.8s5.5.8 5.5 1.8v8c0 1-2.5 1.8-5.5 1.8S2.5 13 2.5 12z M2.5 4c0 1 2.5 1.8 5.5 1.8s5.5-.8 5.5-1.8 M2.5 8c0 1 2.5 1.8 5.5 1.8s5.5-.8 5.5-1.8",
  history: "M2.5 8A5.5 5.5 0 1 0 4 4.2 M2.5 2v3h3 M8 5v3l2 1.5",
  play: "M5 3.5v9l7-4.5z",
  refresh: "M13 8A5 5 0 1 1 11.5 4.5 M13 2v3h-3",
  arrow: "M3 8h10 M9 4l4 4-4 4",
  up: "M8 13V3 M4 7l4-4 4 4",
  plus: "M8 3v10 M3 8h10",
  compare: "M2.5 2.5h4.5v11H2.5z M9 2.5h4.5v11H9z",
  panel: "M2.5 3h11v10h-11z M10 3v10",
  schema: "M2.5 2.5h5v4h-5z M8.5 9.5h5v4h-5z M5 6.5v4.5a.5.5 0 0 0 .5.5h3",
  goal: "M8 14A6 6 0 1 0 8 2a6 6 0 0 0 0 12z M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z M8 8.5v-1",
  explorer: "M2.5 2.5h11v11h-11z M2.5 6h11 M6 6v7.5",
  home: "M2.5 7.2 8 2.5l5.5 4.7v6.3h-3.7V9.8H6.2v3.7H2.5z",
  user: "M8 8a2.8 2.8 0 1 0 0-5.6A2.8 2.8 0 0 0 8 8z M2.5 14c.6-2.8 2.8-4.3 5.5-4.3s4.9 1.5 5.5 4.3",
  signout: "M6 2.5H3.5v11H6 M10 5l3 3-3 3 M6.5 8H13",
};

export function Icon({
  name,
  size = 16,
  className,
}: {
  name: string;
  size?: number;
  className?: string;
}) {
  return (
    <svg
      className={`icon icon-${name} ${className ?? ""}`}
      viewBox="0 0 16 16"
      width={size}
      height={size}
      aria-hidden
    >
      <path
        d={PATHS[name] ?? PATHS.doc}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** The memstore mark: layered memory. */
export function Logo({ size = 26 }: { size?: number }) {
  return (
    <svg className="logo" viewBox="0 0 32 32" width={size} height={size} aria-hidden>
      <rect width="32" height="32" rx="8" fill="var(--accent)" />
      <path d="M8 12.5 16 8l8 4.5-8 4.5z" fill="var(--accent-ink)" />
      <path
        d="m8 16.5 8 4.5 8-4.5"
        fill="none"
        stroke="var(--accent-ink)"
        strokeWidth="1.8"
        strokeLinejoin="round"
        opacity=".75"
      />
      <path
        d="m8 20.5 8 4.5 8-4.5"
        fill="none"
        stroke="var(--accent-ink)"
        strokeWidth="1.8"
        strokeLinejoin="round"
        opacity=".45"
      />
    </svg>
  );
}

/** Placeholder lines while a view loads, shaped like what is coming. */
export function Skeleton({ rows = 4, title = true }: { rows?: number; title?: boolean }) {
  return (
    <div className="skeleton" aria-busy="true" aria-label="Loading">
      {title && <div className="sk sk-title" />}
      {title && <div className="sk sk-sub" />}
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="sk sk-row" style={{ width: `${92 - ((i * 17) % 30)}%` }} />
      ))}
    </div>
  );
}

export function EmptyState({
  icon = "folder",
  title,
  children,
  action,
}: {
  icon?: string;
  title: string;
  children?: React.ReactNode;
  action?: React.ReactNode;
}) {
  return (
    <div className="state">
      <div className="state-icon">
        <Icon name={icon} size={22} />
      </div>
      <h3 className="state-title">{title}</h3>
      {children && <div className="state-body">{children}</div>}
      {action && <div className="state-action">{action}</div>}
    </div>
  );
}

export function ErrorState({
  title = "Something went wrong",
  message,
  onRetry,
}: {
  title?: string;
  message: string;
  onRetry?: () => void;
}) {
  return (
    <div className="state state-error" role="alert">
      <div className="state-icon">
        <Icon name="alert" size={22} />
      </div>
      <h3 className="state-title">{title}</h3>
      <div className="state-body">{message}</div>
      {onRetry && (
        <div className="state-action">
          <button className="btn" onClick={onRetry}>
            <Icon name="refresh" size={14} /> Try again
          </button>
        </div>
      )}
    </div>
  );
}

/** A long id shown short, copied whole. */
export function CopyId({ id, n = 12 }: { id: string; n?: number }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className="copy-id mono"
      title={`${id} (copy)`}
      onClick={() =>
        navigator.clipboard?.writeText(id).then(() => {
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        })
      }
    >
      {id.length > n + 2 ? `${id.slice(0, n)}…` : id}
      <Icon name={done ? "check" : "copy"} size={12} />
    </button>
  );
}
