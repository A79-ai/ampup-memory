// AmpUp Memory for OpenCode and Kilo Code (whose CLI, from v7, is OpenCode's): the team memory's hooks, as the
// Claude Code plugin runs them. Each session opens an AmpUp Memory session at its start, each prompt gets the
// memory's advice and recall for it, each turn's end sends the session so far, a compaction remembers it, and the
// session's end ends it. Everything goes through `ampup-memory-client hook <event> --harness <opencode|kilo>`, which
// takes Claude Code's hook input on stdin, reads the session from a transcript in Claude Code's format that this
// plugin writes (under ~/.cache/ampup-memory/transcripts/), and answers with Claude Code's hook output, whose
// `hookSpecificOutput.additionalContext` goes into the model's system prompt for the rest of the session.
//
// Install: copy this file to ~/.config/opencode/plugins/ (OpenCode) or ~/.config/kilo/plugin/ (Kilo Code), or run
// `ampup-memory-client hooks install --harness opencode` (or `kilo`). It needs ampup-memory-client on the PATH or in
// ~/.local/bin (https://github.com/A79-ai/ampup-memory, plugins/ampup-memory/bin/install.sh) and a sign-in
// (`ampup-memory-client join <server> --org <org>`).
//
// Never in the way: a hook that fails, is missing or takes too long (10 seconds for a prompt) is skipped and the
// session goes on as if the plugin weren't there.
//
//   AMPUP_MEMORY_CLIENT    the client to run (default: ~/.local/bin/ampup-memory-client, else the PATH's)
//   AMPUP_MEMORY_HARNESS   the harness name to send (default: kilo under Kilo Code, else opencode)
//   AMPUP_MEMORY_HOOKS=off turns the plugin off
//
// Only the default export: OpenCode loads every export of a plugin file as a plugin.

import type { Plugin } from "@opencode-ai/plugin"
import { spawn } from "node:child_process"
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs"
import { homedir } from "node:os"
import { basename, delimiter, join } from "node:path"

type Hook = "session-start" | "prompt" | "stop" | "session-end" | "end"

const EVENT: Record<Hook, string> = {
  "session-start": "SessionStart",
  prompt: "UserPromptSubmit",
  stop: "Stop",
  "session-end": "PreCompact",
  end: "SessionEnd",
}

// How long each hook may take before the session goes on without it. The client never waits on the memory itself
// (what it needs is fetched by a background copy and handed on at the next hook), so these are only a backstop.
const TIMEOUT_MS: Record<Hook, number> = {
  "session-start": 15_000,
  prompt: 10_000,
  stop: 15_000,
  "session-end": 15_000,
  end: 5_000,
}

// At most this much of what the hooks added rides in the system prompt (the session's start, then each prompt's,
// newest kept first).
const CONTEXT_MAX = 12_000

type Session = {
  id: string
  started?: Promise<void>
  startContext: string
  promptContexts: string[]
  written: Set<string>
  transcript: string
  prompted: boolean
  ended: boolean
}

const home = () => process.env.HOME || homedir()
const cacheDir = () => join(process.env.XDG_CACHE_HOME || join(home(), ".cache"), "ampup-memory")

// The client: where the install puts it, else the PATH's. Looked up each time, so one installed mid-session is used.
function client(): string | undefined {
  const named = process.env.AMPUP_MEMORY_CLIENT
  if (named) return named
  const dir = process.env.AMPUP_MEMORY_INSTALL_DIR || join(home(), ".local", "bin")
  const candidates = [join(dir, "ampup-memory-client")]
  for (const d of (process.env.PATH || "").split(delimiter)) if (d) candidates.push(join(d, "ampup-memory-client"))
  return candidates.find((p) => existsSync(p))
}

function harnessName(): string {
  if (process.env.AMPUP_MEMORY_HARNESS) return process.env.AMPUP_MEMORY_HARNESS
  const exe = basename(process.execPath || "").toLowerCase()
  const argv0 = basename(process.argv[0] || "").toLowerCase()
  return exe.includes("kilo") || argv0.includes("kilo") ? "kilo" : "opencode"
}

// Runs one hook and resolves with the context it adds ("" for none, or on any failure).
function runHook(harness: string, hook: Hook, input: Record<string, unknown>, cwd: string): Promise<string> {
  const bin = client()
  if (!bin) return Promise.resolve("")
  return new Promise((resolve) => {
    let out = ""
    let done = false
    const finish = () => {
      if (done) return
      done = true
      clearTimeout(timer)
      resolve(contextOf(out))
    }
    let child: ReturnType<typeof spawn>
    try {
      child = spawn(bin, ["hook", hook, "--harness", harness], {
        cwd: existsSync(cwd) ? cwd : undefined,
        stdio: ["pipe", "pipe", "ignore"],
        env: process.env,
      })
    } catch {
      return resolve("")
    }
    const timer = setTimeout(() => {
      try {
        child.kill("SIGKILL")
      } catch {}
      out = ""
      finish()
    }, TIMEOUT_MS[hook])
    child.on("error", () => {
      out = ""
      finish()
    })
    child.on("close", finish)
    child.stdout?.on("data", (d) => (out += d.toString()))
    child.stdin?.on("error", () => {})
    child.stdin?.end(JSON.stringify({ hook_event_name: EVENT[hook], ...input }))
  })
}

// Claude Code's hook output: `hookSpecificOutput.additionalContext` (or Cursor's `additional_context`), else plain
// text, which Claude Code adds as it is.
function contextOf(out: string): string {
  const text = out.trim()
  if (!text) return ""
  try {
    const v = JSON.parse(text)
    const c = v?.hookSpecificOutput?.additionalContext ?? v?.additional_context ?? v?.additionalContext
    return typeof c === "string" ? c : ""
  } catch {
    return text
  }
}

// OpenCode's tools by Claude Code's names and arguments, which the client reads a session's edits from.
function claudeTool(tool: string, input: any): { name: string; input: any } {
  const i = input && typeof input === "object" ? input : {}
  switch (tool) {
    case "edit":
      return { name: "Edit", input: { file_path: i.filePath, old_string: i.oldString, new_string: i.newString } }
    case "write":
      return { name: "Write", input: { file_path: i.filePath, content: i.content } }
    case "read":
      return { name: "Read", input: { file_path: i.filePath, offset: i.offset, limit: i.limit } }
    case "bash":
      return { name: "Bash", input: { command: i.command, description: i.description } }
    case "grep":
      return { name: "Grep", input: i }
    case "glob":
      return { name: "Glob", input: i }
    default:
      return { name: tool, input: i }
  }
}

function resultText(output: unknown): string {
  if (typeof output === "string") return output
  if (output == null) return ""
  try {
    return JSON.stringify(output)
  } catch {
    return String(output)
  }
}

// The session's finished messages not yet in its transcript, appended in Claude Code's JSONL shape (a line per
// prompt, per answer's text, per tool call and per tool result), each once: the lines' uuids say what is there,
// so a restarted OpenCode appends only what is new. An unfinished answer, and everything after it, waits.
async function writeTranscript(sdk: any, s: Session, cwd: string): Promise<void> {
  let messages: any[] = []
  try {
    const r = await sdk.session.messages({ path: { id: s.id } })
    messages = Array.isArray(r) ? r : (r?.data ?? [])
  } catch {
    return
  }
  const lines: string[] = []
  const line = (uuid: string, type: "user" | "assistant", content: any[], at?: number) => {
    if (s.written.has(uuid)) return
    s.written.add(uuid)
    lines.push(
      JSON.stringify({
        type,
        uuid,
        sessionId: s.id,
        cwd,
        timestamp: new Date(at ?? Date.now()).toISOString(),
        message: { role: type, content },
      }),
    )
  }
  for (const m of messages) {
    const info = m?.info ?? {}
    const parts: any[] = m?.parts ?? []
    if (info.role === "assistant" && !info.time?.completed) break
    if (info.role === "user") {
      const text = parts
        .filter((p) => p?.type === "text" && !p.synthetic && !p.ignored && typeof p.text === "string")
        .map((p) => p.text)
        .join("\n")
        .trim()
      if (text) line(`${info.id}`, "user", [{ type: "text", text }], info.time?.created)
      continue
    }
    if (info.role !== "assistant") continue
    parts.forEach((p, n) => {
      const uuid = `${info.id}:${n}`
      if (p?.type === "text" && typeof p.text === "string" && p.text.trim() && !p.synthetic) {
        line(uuid, "assistant", [{ type: "text", text: p.text }], info.time?.created)
      } else if (p?.type === "tool" && (p.state?.status === "completed" || p.state?.status === "error")) {
        const t = claudeTool(p.tool, p.state.input)
        line(uuid, "assistant", [{ type: "tool_use", id: p.callID, name: t.name, input: t.input }], info.time?.created)
        const failed = p.state.status === "error"
        line(`${uuid}:result`, "user", [
          {
            type: "tool_result",
            tool_use_id: p.callID,
            content: failed ? resultText(p.state.error) : resultText(p.state.output),
            ...(failed ? { is_error: true } : {}),
          },
        ])
      }
    })
  }
  if (!lines.length) return
  try {
    appendFileSync(s.transcript, lines.join("\n") + "\n")
  } catch {}
}

function promptText(parts: any[]): string {
  return (parts || [])
    .filter((p) => p?.type === "text" && !p.synthetic && !p.ignored && typeof p.text === "string")
    .map((p) => p.text)
    .join("\n")
    .trim()
}

const AmpUpMemory: Plugin = async ({ client: sdk, directory, worktree }) => {
  if (process.env.AMPUP_MEMORY_HOOKS === "off") return {}
  const harness = harnessName()
  const cwd: string = directory || worktree || process.cwd()
  const transcripts = join(cacheDir(), "transcripts")
  const sessions = new Map<string, Session>()
  // Sessions another session started (a subagent's task): not the person's, so not sessions of their own.
  const children = new Set<string>()

  const sessionOf = (id: string): Session => {
    let s = sessions.get(id)
    if (s) return s
    const transcript = join(transcripts, `${harness}-${id.replace(/[^A-Za-z0-9_.-]/g, "_")}.jsonl`)
    const written = new Set<string>()
    try {
      mkdirSync(transcripts, { recursive: true, mode: 0o700 })
      if (existsSync(transcript)) {
        for (const l of readFileSync(transcript, "utf8").split("\n")) {
          try {
            const uuid = JSON.parse(l)?.uuid
            if (uuid) written.add(uuid)
          } catch {}
        }
      }
    } catch {}
    s = { id, startContext: "", promptContexts: [], written, transcript, prompted: false, ended: false }
    sessions.set(id, s)
    return s
  }

  const input = (s: Session, extra: Record<string, unknown> = {}) => ({
    session_id: s.id,
    cwd,
    transcript_path: s.transcript,
    prompt: "",
    ...extra,
  })

  const start = (s: Session): Promise<void> => {
    s.started ??= runHook(harness, "session-start", input(s, { source: "startup" }), cwd).then((c) => {
      s.startContext = c
    })
    return s.started
  }

  const hook = async (s: Session, h: Hook) => {
    await writeTranscript(sdk, s, cwd)
    await runHook(harness, h, input(s), cwd)
  }

  const end = async (s: Session) => {
    if (s.ended || !s.prompted) return
    s.ended = true
    await hook(s, "end")
  }

  return {
    event: async ({ event }: any) => {
      try {
        const p = event?.properties ?? {}
        switch (event?.type) {
          case "session.created": {
            const info = p.info ?? {}
            if (!info.id) return
            if (info.parentID) {
              children.add(info.id)
              return
            }
            await start(sessionOf(info.id))
            return
          }
          case "session.idle": {
            const id = p.sessionID
            if (!id || children.has(id) || !sessions.get(id)?.prompted) return
            await hook(sessionOf(id), "stop")
            return
          }
          case "session.deleted": {
            const id = p.info?.id
            const s = id && sessions.get(id)
            if (s) await end(s)
            return
          }
        }
      } catch {}
    },

    "chat.message": async (inp: any, out: any) => {
      try {
        const id = inp?.sessionID
        if (!id || children.has(id)) return
        const s = sessionOf(id)
        await start(s)
        s.prompted = true
        s.ended = false
        await writeTranscript(sdk, s, cwd)
        const c = await runHook(harness, "prompt", input(s, { prompt: promptText(out?.parts) }), cwd)
        if (c) s.promptContexts.push(c)
      } catch {}
    },

    "experimental.chat.system.transform": async (inp: any, out: any) => {
      try {
        const id = inp?.sessionID
        const s = id && sessions.get(id)
        if (!s || !Array.isArray(out?.system)) return
        await s.started
        let room = CONTEXT_MAX - s.startContext.length
        const recent: string[] = []
        for (const c of [...s.promptContexts].reverse()) {
          if (c.length > room) break
          recent.unshift(c)
          room -= c.length
        }
        const added = [s.startContext.slice(0, CONTEXT_MAX), ...recent].filter(Boolean)
        if (added.length) out.system.push(added.join("\n\n"))
      } catch {}
    },

    "experimental.session.compacting": async (inp: any) => {
      try {
        const id = inp?.sessionID
        const s = id && sessions.get(id)
        if (s?.prompted) await hook(s, "session-end")
      } catch {}
    },

    dispose: async () => {
      try {
        await Promise.all([...sessions.values()].map(end))
      } catch {}
    },
  }
}

export default AmpUpMemory
