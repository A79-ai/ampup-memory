import { useEffect, useRef, useState } from "react";
import { call } from "./api";
import { errorText, transport } from "../transport";
import { Receipt, type ReceiptFact, type ReceiptItem } from "./receipt";
import type { Focus, Selection } from "./types";
import { Icon } from "./ui";
import "./learn.css";

// Adding to the memory by hand: files dropped in or text pasted (and an Ask answer kept), each kept as an episode
// in the person's own memory (or everyone's who reads it) and read for facts at once, then a receipt of what was
// learned. Memstore's `/learn` and `/learn/status`, through the transport's `learn` area.

/** What memstore keeps at most of one item, in bytes. */
const MAX_BYTES = 64 * 1024;
/** The file types memstore reads (its `/learn` `TEXT_TYPES`); the picker offers these alone. */
const TEXT_TYPES = [
  "txt", "text", "md", "markdown", "csv", "tsv", "json", "jsonl", "ndjson", "yaml", "yml", "log",
];
const ACCEPT = TEXT_TYPES.map((t) => `.${t}`).join(",");
/** Recordings: not read yet, and said so, rather than refused for their size. */
const MEDIA_TYPES = new Set([
  "mp4", "mov", "m4v", "webm", "mkv", "avi", "mp3", "wav", "m4a", "aac", "ogg", "oga", "opus", "flac",
]);

/** Why a file isn't sent at all, by its type, before its size: a recording is never "split it". */
function unreadType(name: string): string | undefined {
  const dot = name.lastIndexOf(".");
  const ext = dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
  if (TEXT_TYPES.includes(ext)) return undefined;
  if (MEDIA_TYPES.has(ext))
    return "audio and video aren't read yet: add a transcript of it as text instead";
  return `${ext ? `.${ext} files aren't` : "files without an extension aren't"} read: text, Markdown, CSV, JSON and YAML are`;
}
/** How often the receipt is read again while extraction runs, and for how long at most. */
const POLL_MS = 1500;
const POLL_FOR_MS = 3 * 60_000;

/** One thing to add, as `/learn` takes it. */
export type LearnItem =
  | { kind: "file"; name: string; base64: string }
  | { kind: "paste"; name?: string; text: string }
  | { kind: "answer"; question: string; answer: string };

type Added = {
  added: {
    name: string;
    kind: string;
    episode_id: string;
    source: string;
    bytes: number;
    new: boolean;
    /** Whether extraction reads it for facts (here, or the schema's contract). */
    read?: boolean;
  }[];
  refused: { name: string; reason: string }[];
  scope: string;
  readers: string;
  extraction: "on" | "off" | "none";
};

type Status = {
  done: boolean;
  extraction: "on" | "off" | "none";
  items: {
    episode_id: string;
    status: ReceiptItem["status"];
    reason?: string;
    facts?: ReceiptFact[];
    entities?: { entity: string; facts: number; new: boolean }[];
    known?: ReceiptFact[];
    refused?: { facts: number; why: string };
  }[];
};

/** Whether the host's transport can add to the memory and read back what it learned. */
export const canLearn = () => {
  try {
    return transport().capabilities.learn === true;
  } catch {
    return false;
  }
};

/** Adds `items` to the memory (`whose`: the caller's alone, or everyone's who reads it). */
export const learn = (sel: Selection, items: LearnItem[], whose: "me" | "team" = "me") =>
  call<Added>(sel, "learn", { op: "add", items, for: whose });

/** The receipt of what was added: kept with what has been learned from it so far, read again until it is all read. */
export function useReceipt(sel: Selection, added: Added | null) {
  const [status, setStatus] = useState<Status | null>(null);
  const [stalled, setStalled] = useState(false);
  const ids = added?.added.map((a) => a.episode_id) ?? [];
  const key = ids.join(",");
  useEffect(() => {
    setStatus(null);
    setStalled(false);
    if (!ids.length) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const started = Date.now();
    const poll = () =>
      call<Status>(sel, "learn", { op: "status", episode_ids: ids })
        .then((s) => {
          if (!live) return;
          setStatus(s);
          if (s.done) return;
          if (Date.now() - started > POLL_FOR_MS) return setStalled(true);
          timer = setTimeout(poll, POLL_MS);
        })
        .catch(() => {
          if (live) timer = setTimeout(poll, POLL_MS * 2);
        });
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [key, sel.org, sel.team]);
  if (!added) return null;
  const byId = new Map(status?.items.map((i) => [i.episode_id, i]));
  const items: ReceiptItem[] = [
    ...added.added.map((a): ReceiptItem => {
      const s = byId.get(a.episode_id);
      const pendingStill = !s || s.status === "pending";
      return {
        name: a.name,
        episodeId: a.episode_id,
        new: a.new,
        status: s ? s.status : (a.read ?? added.extraction === "on") ? "pending" : "not_read",
        reason:
          stalled && pendingStill
            ? "still waiting to be read: its receipt fills in once it is"
            : s?.reason,
        facts: s?.facts ?? [],
        entities: s?.entities ?? [],
        known: s?.known ?? [],
        dropped: s?.refused,
      };
    }),
    ...added.refused.map(
      (r): ReceiptItem => ({
        name: r.name,
        status: "refused",
        reason: r.reason,
        facts: [],
        entities: [],
        known: [],
      }),
    ),
  ];
  // Stalled: shown as kept, not spinning forever.
  const shown = stalled
    ? items.map((i) => (i.status === "pending" ? { ...i, status: "not_read" as const } : i))
    : items;
  return {
    items: shown,
    extraction: status?.extraction ?? added.extraction,
    stalled: stalled && items.some((i) => i.status === "pending"),
  };
}

/** A file as `/learn` takes it (its bytes in base64), or why it isn't sent at all. */
function readFile(f: File): Promise<LearnItem | { refused: string; name: string }> {
  const unread = unreadType(f.name);
  if (unread) return Promise.resolve({ name: f.name, refused: unread });
  if (f.size > MAX_BYTES)
    return Promise.resolve({
      name: f.name,
      refused: `it's ${Math.ceil(f.size / 1024)} KB: at most ${MAX_BYTES / 1024} KB is added at a time, so split it`,
    });
  return new Promise((resolve) => {
    const r = new FileReader();
    r.onload = () => {
      const url = String(r.result ?? "");
      resolve({ kind: "file", name: f.name, base64: url.slice(url.indexOf(",") + 1) });
    };
    r.onerror = () => resolve({ name: f.name, refused: "it couldn't be read" });
    r.readAsDataURL(f);
  });
}

export type AddToMemoryProps = {
  sel: Selection;
  /** Opens what the receipt links to (an entity, an episode); without it, its links move the page's hash. */
  onOpen?: (f: Focus) => void;
  /** Called with each request's answer once it is kept (before it is read): a page may refresh its listing. */
  onAdded?: () => void;
  /** A heading of its own; by default "Add to memory". */
  title?: string;
};

/**
 * Add to memory: drop files or paste text, choose whose it is, and see the receipt of what was learned. For the
 * Explorer's Sources and any page that wants a builder to feed the memory before a source is connected.
 */
export function AddToMemory({ sel, onOpen, onAdded, title = "Add to memory" }: AddToMemoryProps) {
  const [files, setFiles] = useState<File[]>([]);
  const [text, setText] = useState("");
  const [whose, setWhose] = useState<"me" | "team">("me");
  const [over, setOver] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [added, setAdded] = useState<Added | null>(null);
  const picker = useRef<HTMLInputElement>(null);
  const receipt = useReceipt(sel, added);

  const take = (list: FileList | null) => {
    if (!list?.length) return;
    setFiles((fs) => [
      ...fs,
      ...[...list].filter((f) => !fs.some((g) => g.name === f.name && g.size === f.size)),
    ]);
  };

  const submit = async () => {
    setBusy(true);
    setError(undefined);
    try {
      const read = await Promise.all(files.map(readFile));
      const items: LearnItem[] = read.filter((r): r is LearnItem => "kind" in r);
      const refusedHere = read
        .filter((r): r is { refused: string; name: string } => "refused" in r)
        .map((r) => ({ name: r.name, reason: r.refused }));
      if (text.trim()) items.push({ kind: "paste", text });
      const answer: Added = items.length
        ? await learn(sel, items, whose)
        : { added: [], refused: [], scope: "", readers: "", extraction: "on" };
      setAdded({ ...answer, refused: [...answer.refused, ...refusedHere] });
      setFiles([]);
      setText("");
      onAdded?.();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const empty = !files.length && !text.trim();
  return (
    <div className="learn">
      <section className="card card-pad learn-card" aria-label={title}>
        <div className="learn-head">
          <span className="kind kind-sm k-doc">
            <Icon name="database" size={13} />
          </span>
          <div>
            <div className="learn-title">{title}</div>
            <div className="muted learn-sub">
              Drop in notes, docs or exports, or paste text. It is read for facts right away, and you
              see what was learned.
            </div>
          </div>
        </div>
        <div
          className={`learn-drop ${over ? "is-over" : ""}`}
          role="button"
          tabIndex={0}
          aria-label="Choose files to add, or drop them here"
          onClick={() => picker.current?.click()}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              picker.current?.click();
            }
          }}
          onDragOver={(e) => {
            e.preventDefault();
            setOver(true);
          }}
          onDragLeave={() => setOver(false)}
          onDrop={(e) => {
            e.preventDefault();
            setOver(false);
            take(e.dataTransfer.files);
          }}
        >
          <Icon name="doc" size={18} />
          <span>
            <strong>Drop files here</strong> or click to choose
          </span>
          <span className="muted learn-types">
            Text, Markdown, CSV, JSON or YAML, up to {MAX_BYTES / 1024} KB each
          </span>
          <input
            ref={picker}
            type="file"
            multiple
            accept={ACCEPT}
            className="learn-file"
            onChange={(e) => {
              take(e.target.files);
              e.target.value = "";
            }}
          />
        </div>
        {files.length > 0 && (
          <ul className="learn-files">
            {files.map((f) => (
              <li key={`${f.name}${f.size}`} className="chip">
                <Icon name="doc" size={12} />
                {f.name}
                <span className="chip-type">{Math.max(1, Math.round(f.size / 1024))} KB</span>
                <button
                  type="button"
                  className="learn-x"
                  aria-label={`Remove ${f.name}`}
                  onClick={() => setFiles((fs) => fs.filter((g) => g !== f))}
                >
                  <Icon name="close" size={11} />
                </button>
              </li>
            ))}
          </ul>
        )}
        <textarea
          className="args learn-paste"
          rows={4}
          placeholder="Or paste text: a decision, meeting notes, how something works…"
          aria-label="Text to add"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !empty && !busy) submit();
          }}
        />
        {error && <div className="error-text">{error}</div>}
        <div className="learn-row">
          <div className="learn-whose" role="radiogroup" aria-label="Who it is for">
            {(
              [
                ["me", "Just me"],
                ["team", "Everyone in this memory"],
              ] as const
            ).map(([v, label]) => (
              <button
                key={v}
                type="button"
                role="radio"
                aria-checked={whose === v}
                className={`chip ${whose === v ? "chip-on" : ""}`}
                onClick={() => setWhose(v)}
              >
                {label}
              </button>
            ))}
          </div>
          <button
            className="btn btn-primary"
            type="button"
            disabled={empty || busy}
            onClick={submit}
          >
            <Icon name="play" size={13} /> {busy ? "Adding…" : "Add to memory"}
          </button>
        </div>
      </section>
      {added && receipt && (
        <Receipt
          items={receipt.items}
          extraction={receipt.extraction}
          stalled={receipt.stalled}
          onOpen={onOpen}
          onDismiss={() => setAdded(null)}
        />
      )}
    </div>
  );
}

/** "Remember this" under an answer: keeps the question and its answer in the person's own memory, and shows what
 * the memory made of it. */
export function RememberThis({
  sel,
  question,
  answer,
}: {
  sel: Selection;
  question: string;
  answer: string;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [added, setAdded] = useState<Added | null>(null);
  const receipt = useReceipt(sel, added);
  if (!canLearn() || !answer.trim()) return null;
  if (added && receipt)
    return (
      <div className="learn-inline">
        <Receipt
          title="Remembered"
          items={receipt.items}
          extraction={receipt.extraction}
          stalled={receipt.stalled}
          compact
        />
      </div>
    );
  return (
    <div className="learn-inline">
      <button
        type="button"
        className="btn learn-remember"
        disabled={busy}
        title="Keep this question and answer in your memory, and see what it learns from it"
        onClick={() => {
          setBusy(true);
          setError(undefined);
          learn(sel, [{ kind: "answer", question, answer }], "me")
            .then(setAdded, (e) => setError(errorText(e)))
            .finally(() => setBusy(false));
        }}
      >
        <Icon name="check" size={13} /> {busy ? "Remembering…" : "Remember this"}
      </button>
      {error && <span className="error-text">{error}</span>}
    </div>
  );
}

/** Add to memory behind a button, for a page whose own content comes first (the Explorer's Sources). Open from
 * the start with `open`. */
export function AddToMemoryButton({ open: openAtFirst = false, ...props }: AddToMemoryProps & { open?: boolean }) {
  const [open, setOpen] = useState(openAtFirst);
  if (!canLearn()) return null;
  if (open) return <AddToMemory {...props} />;
  return (
    <div className="learn-inline learn-open">
      <button type="button" className="btn" onClick={() => setOpen(true)}>
        <Icon name="doc" size={13} /> Add to memory
      </button>
      <span className="muted learn-sub">Upload files or paste text, and see what it learns.</span>
    </div>
  );
}

/**
 * Add to memory as one button that opens the form in a dialog, for a page whose own layout leaves no room for
 * it (Home's quick actions and its first steps). Leaving the page, as the receipt's links do, closes it.
 */
export function AddToMemoryDialog(props: AddToMemoryProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [open, setOpen] = useState(false);
  useEffect(() => {
    const d = dialog.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  if (!canLearn()) return null;
  return (
    <>
      <button type="button" className="btn" onClick={() => setOpen(true)}>
        <Icon name="doc" size={14} /> Add to memory
      </button>
      <dialog
        ref={dialog}
        className="learn-dialog"
        aria-label={props.title ?? "Add to memory"}
        onClose={() => setOpen(false)}
        onClick={(e) => e.target === e.currentTarget && setOpen(false)}
      >
        <button
          type="button"
          className="learn-dialog-x"
          aria-label="Close"
          onClick={() => setOpen(false)}
        >
          <Icon name="close" size={12} />
        </button>
        {open && <AddToMemory {...props} />}
      </dialog>
    </>
  );
}
