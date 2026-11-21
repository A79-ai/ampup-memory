// AmpUp Memory for Amp: the team memory's hooks, as the Claude Code plugin runs them. Each thread opens an AmpUp
// Memory session when it starts, each prompt gets the memory's advice and recall for it (added to the prompt,
// hidden from the thread's view), each turn's end sends the thread so far, and the session ends when the thread is
// left or Amp exits. Everything goes through `ampup-memory-client hook <event> --harness amp`, which takes Claude
// Code's hook input on stdin, reads the thread from a transcript in Claude Code's format that this plugin writes
// (under ~/.cache/ampup-memory/transcripts/), and answers with Claude Code's hook output
// (`hookSpecificOutput.additionalContext`).
//
// Install: copy this file to ~/.config/amp/plugins/ (or a project's .amp/plugins/), or run
// `ampup-memory-client hooks install --harness amp`, then `plugins: reload` in a running Amp. It needs
// ampup-memory-client on the PATH or in ~/.local/bin (https://github.com/A79-ai/ampup-memory,
// plugins/ampup-memory/bin/install.sh) and a sign-in (`ampup-memory-client join <server> --org <org>`).
//
// Never in the way: a hook that fails, is missing or takes too long (10 seconds for a prompt) is skipped and the
// thread goes on as if the plugin weren't there. It runs the client with node:child_process rather than amp.$,
// which can't take stdin or a timeout.
//
//   AMPUP_MEMORY_CLIENT    the client to run (default: ~/.local/bin/ampup-memory-client, else the PATH's)
//   AMPUP_MEMORY_HOOKS=off turns the plugin off

import type { PluginAPI } from "@ampcode/plugin"
import { spawn } from "node:child_process"
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs"
import { homedir } from "node:os"
import { delimiter, join } from "node:path"

export const description =
  "AmpUp Memory: the team memory's advice and recall at each prompt, and each thread recorded as a session it learns from."

type Hook = "session-start" | "prompt" | "stop" | "session-end" | "end"

const EVENT: Record<Hook, string> = {
  "session-start": "SessionStart",
  prompt: "UserPromptSubmit",
  stop: "Stop",
  "session-end": "PreCompact",
  end: "SessionEnd",
}

// How long each hook may take before the thread goes on without it. The client never waits on the memory itself
// (what it needs is fetched by a background copy and handed on at the next hook), so these are only a backstop.
// The end is short: Amp gives a plugin's dispose about three seconds, and the client hands the end to a background
// copy of itself.
const TIMEOUT_MS: Record<Hook, number> = {
  "session-start": 15_000,
  prompt: 10_000,
  stop: 15_000,
  "session-end": 15_000,
  end: 2_500,
}

const HARNESS = "amp"

type Thread = {
  id: string
  started?: Promise<void>
  startContext: string
  startHanded: boolean
  injected: string[]
  written: Set<string>
  transcript: string
  prompted: boolean
  ended: boolean
}

const home = () => process.env.HOME || homedir()
const cacheDir = () => join(process.env.XDG_CACHE_HOME || join(home(), ".cache"), "ampup-memory")

// The client: where the install puts it, else the PATH's. Looked up each time, so one installed mid-thread is used.
function client(): string | undefined {
  const named = process.env.AMPUP_MEMORY_CLIENT
  if (named) return named
  const dir = process.env.AMPUP_MEMORY_INSTALL_DIR || join(home(), ".local", "bin")
  const candidates = [join(dir, "ampup-memory-client")]
  for (const d of (process.env.PATH || "").split(delimiter)) if (d) candidates.push(join(d, "ampup-memory-client"))
  return candidates.find((p) => existsSync(p))
}

// Runs one hook and resolves with the context it adds ("" for none, or on any failure).
function runHook(hook: Hook, input: Record<string, unknown>, cwd: string): Promise<string> {
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
      child = spawn(bin, ["hook", hook, "--harness", HARNESS], {
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

// Amp's tools by Claude Code's names and arguments, which the client reads a session's edits from. Which call
// edits which file, and which runs a shell command, is Amp's own helpers' answer; the arguments' names are a guess
// kept only where the helpers say nothing.
type Helpers = {
  shellCommandFromToolCall: (call: any) => { command: string; dir?: string } | null
  filesModifiedByToolCall: (call: any) => unknown[] | null
  filePathFromURI: (uri: any) => string
}

function claudeTool(helpers: Helpers | undefined, id: string, tool: string, input: any): { name: string; input: any } {
  const i = input && typeof input === "object" ? input : {}
  const call = { toolUseID: id, tool, input: i }
  try {
    const shell = helpers?.shellCommandFromToolCall(call)
    if (shell?.command) return { name: "Bash", input: { command: shell.command, cwd: shell.dir } }
    const files = helpers?.filesModifiedByToolCall(call)
    if (files?.length === 1) {
      const file_path = helpers!.filePathFromURI(files[0])
      if (tool === "create_file") return { name: "Write", input: { file_path, content: i.content } }
      return { name: "Edit", input: { file_path, old_string: i.old_str, new_string: i.new_str ?? i.content, ...i } }
    }
  } catch {}
  switch (tool) {
    case "edit_file":
      return { name: "Edit", input: { file_path: i.path, old_string: i.old_str, new_string: i.new_str } }
    case "create_file":
      return { name: "Write", input: { file_path: i.path, content: i.content } }
    case "Bash":
      return { name: "Bash", input: { command: i.cmd ?? i.command, cwd: i.cwd } }
    default:
      return { name: tool, input: i }
  }
}

function resultText(output: unknown): string {
  if (typeof output === "string") return output
  if (Array.isArray(output)) {
    return output.map((b: any) => (b?.type === "text" ? b.text : `[${b?.type ?? "block"}]`)).join("\n")
  }
  if (output == null) return ""
  try {
    return JSON.stringify(output)
  } catch {
    return String(output)
  }
}

// What the person typed: the user message's text less what this plugin added to it (Amp appends a hook's message
// to the user's content), so the memory never takes its own advice for the person's words.
function typed(text: string, injected: string[]): string {
  let t = text
  for (const c of injected) if (c) t = t.split(c).join("")
  return t.trim()
}

// The turn's messages not yet in the thread's transcript, appended in Claude Code's JSONL shape (a line per
// prompt, per answer's text, per tool call and per tool result), each once: the lines' uuids say what is there, so
// a restarted Amp appends only what is new.
function writeTranscript(helpers: Helpers | undefined, t: Thread, messages: any[], cwd: string): void {
  const lines: string[] = []
  const line = (uuid: string, type: "user" | "assistant", content: any[], at?: string) => {
    if (t.written.has(uuid)) return
    t.written.add(uuid)
    lines.push(
      JSON.stringify({
        type,
        uuid,
        sessionId: t.id,
        cwd,
        timestamp: at || new Date().toISOString(),
        message: { role: type, content },
      }),
    )
  }
  for (const m of messages || []) {
    const blocks: any[] = Array.isArray(m?.content) ? m.content : []
    const id = `${t.id}:${m?.id}`
    if (m?.role === "user") {
      const text = typed(
        blocks
          .filter((b) => b?.type === "text" && typeof b.text === "string")
          .map((b) => b.text)
          .join("\n"),
        t.injected,
      )
      if (text) line(id, "user", [{ type: "text", text }], m.createdAt)
      blocks.forEach((b, n) => {
        if (b?.type !== "tool_result" || b.status === "running" || b.status === "pending") return
        const failed = b.status !== "done"
        line(`${id}:${n}`, "user", [
          {
            type: "tool_result",
            tool_use_id: b.toolUseID,
            content: resultText(b.output),
            ...(failed ? { is_error: true } : {}),
          },
        ], m.createdAt)
      })
    } else if (m?.role === "assistant") {
      blocks.forEach((b, n) => {
        if (b?.type === "text" && typeof b.text === "string" && b.text.trim()) {
          line(`${id}:${n}`, "assistant", [{ type: "text", text: b.text }], m.createdAt)
        } else if (b?.type === "tool_use") {
          const tool = claudeTool(helpers, b.id, b.name, b.input)
          line(`${id}:${n}`, "assistant", [{ type: "tool_use", id: b.id, name: tool.name, input: tool.input }], m.createdAt)
        }
      })
    }
  }
  if (!lines.length) return
  try {
    appendFileSync(t.transcript, lines.join("\n") + "\n")
  } catch {}
}

export default function (amp: PluginAPI) {
  if (process.env.AMPUP_MEMORY_HOOKS === "off") return
  const transcripts = join(cacheDir(), "transcripts")
  const threads = new Map<string, Thread>()
  let current: string | undefined

  const cwd = (): string => {
    try {
      const root = amp.system?.workspaceRoot
      if (root) return amp.helpers.filePathFromURI(root)
    } catch {}
    return process.cwd()
  }

  const threadOf = (id: string): Thread => {
    let t = threads.get(id)
    if (t) return t
    const transcript = join(transcripts, `${HARNESS}-${id.replace(/[^A-Za-z0-9_.-]/g, "_")}.jsonl`)
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
    t = { id, startContext: "", startHanded: false, injected: [], written, transcript, prompted: false, ended: false }
    threads.set(id, t)
    return t
  }

  const input = (t: Thread, extra: Record<string, unknown> = {}) => ({
    session_id: t.id,
    cwd: cwd(),
    transcript_path: t.transcript,
    prompt: "",
    ...extra,
  })

  const start = (t: Thread): Promise<void> => {
    t.started ??= runHook("session-start", input(t, { source: "startup" }), cwd()).then((c) => {
      t.startContext = c
    })
    return t.started
  }

  const end = async (t: Thread) => {
    if (t.ended || !t.prompted) return
    t.ended = true
    // Back to this thread later, its session starts again (the memory continues it) and its context comes again.
    t.started = undefined
    t.startHanded = false
    t.prompted = false
    await runHook("end", input(t), cwd())
  }

  // A thread opened, or switched to: its session starts (a resumed one continues), and the one left ends.
  amp.on("session.start", async (event) => {
    try {
      const id = event?.thread?.id
      if (!id) return
      if (current && current !== id) {
        const left = threads.get(current)
        if (left) await end(left)
      }
      current = id
      const t = threadOf(id)
      t.ended = false
      await start(t)
    } catch {}
  })

  // Each prompt: the memory's context for it rides with the prompt, hidden from the thread's view.
  amp.on("agent.start", async (event) => {
    try {
      const id = event?.thread?.id
      if (!id) return {}
      const t = threadOf(id)
      current = id
      t.ended = false
      await start(t)
      t.prompted = true
      const prompted = await runHook("prompt", input(t, { prompt: event.message ?? "" }), cwd())
      const added = [t.startHanded ? "" : t.startContext, prompted].filter(Boolean).join("\n\n")
      t.startHanded = true
      if (!added) return {}
      t.injected.push(added)
      return { message: { content: added, display: false } }
    } catch {
      return {}
    }
  })

  // Each turn's end: the turn joins the transcript and the memory gets the session so far.
  amp.on("agent.end", async (event) => {
    try {
      const id = event?.thread?.id
      if (!id) return
      const t = threadOf(id)
      writeTranscript(amp.helpers as unknown as Helpers | undefined, t, event.messages ?? [], cwd())
      await runHook("stop", input(t), cwd())
    } catch {}
    return
  })

  amp.onDispose(async () => {
    try {
      await Promise.all([...threads.values()].map(end))
    } catch {}
  })
}
