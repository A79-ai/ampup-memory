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
/** Recordings: sent in parts and transcribed where memstore is set to (it says why not where it isn't). */
const MEDIA_TYPES = [
  "mp4", "mov", "m4v", "webm", "mp3", "wav", "m4a", "aac", "ogg", "oga", "opus", "flac",
];
const ACCEPT = [...TEXT_TYPES, ...MEDIA_TYPES].map((t) => `.${t}`).join(",");
/** The bytes in each part of a recording but its last, as memstore's `/learn` takes them (`upload::PART`). */
const PART = 3 * 1024 * 1024;

const extOf = (name: string) => {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
};
const isRecording = (name: string) => MEDIA_TYPES.includes(extOf(name));

/** Why a file isn't sent at all, by its type, before its size: a recording is never "split it". */
function unreadType(name: string): string | undefined {
  const ext = extOf(name);
  if (TEXT_TYPES.includes(ext) || MEDIA_TYPES.includes(ext)) return undefined;
  return `${ext ? `.${ext} files aren't` : "files without an extension aren't"} read: text, Markdown, CSV, JSON, YAML, audio and video are`;
}
/** How often the receipt is read again while extraction runs, and for how long at most; longer while a recording
 * is transcribed. */
const POLL_MS = 1500;
const POLL_FOR_MS = 3 * 60_000;
const TRANSCRIBE_POLL_MS = 3000;
const TRANSCRIBE_FOR_MS = 35 * 60_000;

/** One thing to add, as `/learn` takes it. */
export type LearnItem =
  | { kind: "file"; name: string; base64: string }
  | { kind: "paste"; name?: string; text: string }
  | { kind: "answer"; question: string; answer: string }
  | { kind: "part"; name: string; size: number; n: number; base64: string; upload_id?: string };

/** A recording sent in parts, as memstore answers its last: transcribing apart, under this id. */
type Sent = { name: string; upload_id: string };

/** How a recording sent in parts stands (`/learn/status` `uploads`). */
type Upload = {
  upload_id: string;
  status: "transcribing" | "added" | "refused" | "unknown";
  episode_id?: string;
  new?: boolean;
  reason?: string;
};

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
  /** Parts of a recording taken, the last not yet sent. */
  uploading?: (Sent & { n: number; parts: number })[];
  /** Recordings all sent, being transcribed. */
  transcribing?: Sent[];
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
  uploads?: Upload[];
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

/** The receipt of what was added: kept with what has been learned from it so far, read again until it is all read.
 * A recording being transcribed shows as such until it is kept, then as any other item. */
export function useReceipt(sel: Selection, added: Added | null) {
  const [status, setStatus] = useState<Status | null>(null);
  const [uploads, setUploads] = useState<Map<string, Upload>>(new Map());
  const [stalled, setStalled] = useState(false);
  const ids = added?.added.map((a) => a.episode_id) ?? [];
  const sent = added?.transcribing ?? [];
  const key = `${ids.join(",")}|${sent.map((t) => t.upload_id).join(",")}`;
  useEffect(() => {
    setStatus(null);
    setUploads(new Map());
    setStalled(false);
    if (!ids.length && !sent.length) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const started = Date.now();
    const ups = new Map<string, Upload>();
    const limit = sent.length ? TRANSCRIBE_FOR_MS : POLL_FOR_MS;
    const every = sent.length ? TRANSCRIBE_POLL_MS : POLL_MS;
    const poll = () => {
      const episodeIds = [
        ...ids,
        ...[...ups.values()].flatMap((u) => (u.status === "added" && u.episode_id ? [u.episode_id] : [])),
      ];
      const uploadIds = sent
        .map((t) => t.upload_id)
        .filter((id) => (ups.get(id)?.status ?? "transcribing") === "transcribing");
      call<Status>(sel, "learn", {
        op: "status",
        ...(episodeIds.length ? { episode_ids: episodeIds } : {}),
        ...(uploadIds.length ? { upload_ids: uploadIds } : {}),
      })
        .then((s) => {
          if (!live) return;
          for (const u of s.uploads ?? []) ups.set(u.upload_id, u);
          setUploads(new Map(ups));
          setStatus(s);
          // A recording just kept: its episode is read about next time.
          const justKept = (s.uploads ?? []).some(
            (u) => u.status === "added" && u.episode_id && !episodeIds.includes(u.episode_id),
          );
          if (s.done && !justKept) return;
          if (Date.now() - started > limit) return setStalled(true);
          timer = setTimeout(poll, every);
        })
        .catch(() => {
          if (live) timer = setTimeout(poll, every * 2);
        });
    };
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [key, sel.org, sel.team]);
  if (!added) return null;
  const byId = new Map(status?.items.map((i) => [i.episode_id, i]));
  const kept = (name: string, episodeId: string, isNew: boolean, read?: boolean): ReceiptItem => {
    const s = byId.get(episodeId);
    const pendingStill = !s || s.status === "pending";
    return {
      name,
      episodeId,
      new: isNew,
      status: s ? s.status : (read ?? added.extraction === "on") ? "pending" : "not_read",
      reason:
        stalled && pendingStill ? "still waiting to be read: its receipt fills in once it is" : s?.reason,
      facts: s?.facts ?? [],
      entities: s?.entities ?? [],
      known: s?.known ?? [],
      dropped: s?.refused,
    };
  };
  const refused = (name: string, reason: string): ReceiptItem => ({
    name,
    status: "refused",
    reason,
    facts: [],
    entities: [],
    known: [],
  });
  const items: ReceiptItem[] = [
    ...added.added.map((a) => kept(a.name, a.episode_id, a.new, a.read)),
    ...sent.map((t): ReceiptItem => {
      const u = uploads.get(t.upload_id);
      if (u?.status === "added" && u.episode_id) return kept(t.name, u.episode_id, u.new ?? true);
      if (u?.status === "refused") return refused(t.name, u.reason ?? "it couldn't be transcribed");
      if (u?.status === "unknown") return refused(t.name, "its transcription wasn't found: add it again");
      return {
        name: t.name,
        status: "transcribing",
        reason: stalled ? "still transcribing: add it again later if it doesn't show" : undefined,
        facts: [],
        entities: [],
        known: [],
      };
    }),
    ...added.refused.map((r) => refused(r.name, r.reason)),
  ];
  // Stalled: shown as kept, not spinning forever.
  const shown = stalled
    ? items.map((i) => (i.status === "pending" ? { ...i, status: "not_read" as const } : i))
    : items;
  return {
    items: shown,
    extraction: status?.extraction ?? added.extraction,
    stalled: stalled && items.some((i) => i.status === "pending" || i.status === "transcribing"),
  };
}

/** A blob's bytes in base64. */
function base64Of(b: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const url = String(r.result ?? "");
      resolve(url.slice(url.indexOf(",") + 1));
    };
    r.onerror = () => reject(new Error("it couldn't be read"));
    r.readAsDataURL(b);
  });
}

/** Send a recording in parts (each tried twice), saying how far it has got: answered with what it is transcribed
 * as, or why it was refused. */
async function sendRecording(
  sel: Selection,
  f: File,
  whose: "me" | "team",
  progress: (n: number, of: number) => void,
): Promise<{ transcribing: Sent } | { refused: { name: string; reason: string } }> {
  const parts = Math.max(1, Math.ceil(f.size / PART));
  let uploadId: string | undefined;
  for (let n = 0; n < parts; n++) {
    progress(n + 1, parts);
    let base64: string;
    try {
      base64 = await base64Of(f.slice(n * PART, (n + 1) * PART));
    } catch {
      return { refused: { name: f.name, reason: "it couldn't be read" } };
    }
    const item: LearnItem = {
      kind: "part",
      name: f.name,
      size: f.size,
      n,
      base64,
      ...(uploadId ? { upload_id: uploadId } : {}),
    };
    const a = await learn(sel, [item], whose).catch(() => learn(sel, [item], whose));
    if (a.refused.length) return { refused: { name: f.name, reason: a.refused[0]!.reason } };
    if (a.transcribing?.length) return { transcribing: a.transcribing[0]! };
    uploadId = a.uploading?.[0]?.upload_id;
    if (!uploadId) return { refused: { name: f.name, reason: "it wasn't taken: add it again" } };
  }
  return { refused: { name: f.name, reason: "it didn't finish uploading: add it again" } };
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
  const [progress, setProgress] = useState<string>();
  const [error, setError] = useState<string>();
  const [added, setAdded] = useState<Added | null>(null);
  const picker = useRef<HTMLInputElement>(null);
  const receipt = useReceipt(sel, added);

  const take = (list: FileList | null) => {
    if (!list?.length) return;
    // Copied now: the picker's list is emptied (to pick the same file again) before React runs the update.
    const picked = [...list];
    setFiles((fs) => [
      ...fs,
      ...picked.filter((f) => !fs.some((g) => g.name === f.name && g.size === f.size)),
    ]);
  };

  const submit = async () => {
    setBusy(true);
    setError(undefined);
    try {
      const read = await Promise.all(files.filter((f) => !isRecording(f.name)).map(readFile));
      const items: LearnItem[] = read.filter((r): r is LearnItem => "kind" in r);
      const refusedHere = read
        .filter((r): r is { refused: string; name: string } => "refused" in r)
        .map((r) => ({ name: r.name, reason: r.refused }));
      // Recordings one at a time, in parts.
      const transcribing: Sent[] = [];
      for (const f of files.filter((f) => isRecording(f.name))) {
        const r = await sendRecording(sel, f, whose, (n, of) =>
          setProgress(of > 1 ? `Uploading ${f.name}: ${n} of ${of}…` : `Uploading ${f.name}…`),
        );
        if ("transcribing" in r) transcribing.push(r.transcribing);
        else refusedHere.push(r.refused);
      }
      setProgress(undefined);
      if (text.trim()) items.push({ kind: "paste", text });
      const answer: Added = items.length
        ? await learn(sel, items, whose)
        : { added: [], refused: [], scope: "", readers: "", extraction: "on" };
      setAdded({ ...answer, transcribing, refused: [...answer.refused, ...refusedHere] });
      setFiles([]);
      setText("");
      onAdded?.();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
      setProgress(undefined);
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
              Drop in notes, docs, exports or recordings, or paste text. It is read for facts right
              away, and you see what was learned.
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
            Text, Markdown, CSV, JSON or YAML, up to {MAX_BYTES / 1024} KB each, or an audio or
            video recording to transcribe
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
                <span className="chip-type">
                  {f.size >= 1024 * 1024
                    ? `${(f.size / (1024 * 1024)).toFixed(1)} MB`
                    : `${Math.max(1, Math.round(f.size / 1024))} KB`}
                </span>
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
            <Icon name="play" size={13} /> {busy ? (progress ?? "Adding…") : "Add to memory"}
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
