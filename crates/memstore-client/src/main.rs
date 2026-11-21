//! `ampup-memory-client` (once `memstore-client`, still a link to it): what a person's machine runs to reach a
//! team's memory, without the store and server that make `ampup-memory` hundreds of MB. Its commands are
//! `ampup-memory`'s own, at the same names: `join` to sign in, `hooks install` to register the hooks in a
//! coding agent, `hook` for a coding agent's hooks, `mcp` for the server's MCP endpoint over stdio, `doctor` to check it all.

use anyhow::Result;
use clap::{Parser, Subcommand};
use memstore_client::{client, hook, hook_install, naming, update};

/// The release this was built as (`v0.1.0-rc.7`, from the release workflow), else the crate's version: the
/// plugin compares it with the latest release to keep the client current.
const VERSION: &str = match naming::RELEASE {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(
    name = "ampup-memory-client",
    bin_name = "ampup-memory-client",
    version = VERSION,
    about = "Reach a team's AmpUp Memory from this machine"
)]
struct Cli {
    /// Use this context for one command (also AMPUP_MEMORY_CONTEXT)
    #[arg(long, global = true)]
    context: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in to a server as yourself and add a context for an org (and team) there: the one you're in, or
    /// one you choose; in none yet, name the org to ask to join (its admins are emailed to approve)
    Join {
        server: String,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        no_browser: bool,
        /// Sign in to the org's own Auth0 Organization (its enterprise connection) rather than as yourself
        #[arg(long)]
        sso: bool,
        /// In no org yet: make your own, where the server offers sign-up, without asking first
        #[arg(long)]
        create_org: bool,
        /// Sign in with your personal API key (msk_…) instead of a browser: a cloud agent's machine, CI
        #[arg(long, env = "AMPUP_MEMORY_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
    },
    /// Register AmpUp Memory's hooks and MCP server in a coding agent's user config: `hooks install --harness
    /// <agent>`, `hooks uninstall --harness <agent>`. The agents: codex, gemini, cursor, copilot, droid, qwen,
    /// hermes, kiro, devin, cline, opencode, amp and kilo (Claude Code and Grok Bot install a plugin instead).
    /// The user's own entries are kept, each file is backed up before its first change, and installing twice
    /// changes nothing
    Hooks {
        /// install or uninstall
        action: String,
        /// The agent: codex, gemini, cursor, copilot, droid, qwen, hermes, kiro, devin, cline, opencode, amp or kilo
        #[arg(long)]
        harness: String,
        /// The server, written into the hooks and the MCP entry (else AMPUP_MEMORY_URL, else the context `join`
        /// made, read when they run)
        #[arg(long)]
        url: Option<String>,
        /// The org whose memory the MCP entry reaches (else AMPUP_MEMORY_ORG, else the context's)
        #[arg(long)]
        org: Option<String>,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// A coding agent's hook: session-start, prompt, stop, session-end or status, reading the hook's JSON on stdin;
    /// `outcome good|bad [why]` rates the session's run. Never fails the session: errors go to stderr and it exits 0
    Hook {
        event: String,
        /// outcome: good or bad, then why
        args: Vec<String>,
        /// The agent it runs in: claude-code, codex, gemini, cursor, copilot, grok, droid, qwen, hermes, kiro,
        /// devin, cline, opencode, amp or kilo
        #[arg(long, default_value = "claude-code")]
        harness: String,
        /// outcome: the session to rate (else CLAUDE_CODE_SESSION_ID, else the one last active in this repo)
        #[arg(long)]
        session: Option<String>,
    },
    /// The server's MCP endpoint over stdio, signed in as `join` left you. AMPUP_MEMORY_URL and AMPUP_MEMORY_TOKEN override
    Mcp,
    /// Whether this machine's memory works: the server, who is signed in, the org and how the last hook went
    Doctor,
    /// One of the memory's named metrics as JSON, answered as you see the memory (AMPUP_MEMORY_URL, AMPUP_MEMORY_TOKEN
    /// and AMPUP_MEMORY_ORG sign a service in)
    Metric {
        name: String,
        /// Cite each number: the facts behind it, with their quotes and source episodes
        #[arg(long)]
        evidence: bool,
        /// At most this many cited facts (or counted members) a row
        #[arg(long)]
        limit: Option<u64>,
        /// Filters, as the query language's where clauses (JSON): `[{"attr": "owner", "eq": "rep:ann"}]`
        #[arg(long = "where")]
        filters: Option<String>,
        /// Answer it anew: without it, the answer kept since it was last computed (`computed_at`), however
        /// many facts came since
        #[arg(long)]
        refresh: bool,
    },
    /// Install the newest release over this binary, if it is newer (a session's start runs this in the background)
    #[command(hide = true)]
    Update,
}

fn main() -> Result<()> {
    // The old names (MEMSTORE_*, ~/.memstore) read as the new ones, before anything reads them.
    // SAFETY: first in main, before the runtime: no other thread runs yet.
    unsafe { naming::startup() };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(c) = &cli.context {
        client::override_context(c);
    }
    match cli.command {
        Cmd::Join {
            server,
            org,
            team,
            no_browser,
            sso,
            create_org,
            api_key,
        } => {
            if let Some(key) = api_key {
                return client::join_with_key(&server, org.as_deref(), team.as_deref(), &key).await;
            }
            update::before_sign_in(VERSION).await;
            client::join(
                &server,
                org.as_deref(),
                team.as_deref(),
                no_browser,
                sso,
                create_org,
            )
            .await
        }
        Cmd::Hook {
            event,
            args,
            harness,
            session,
        } => {
            if event == "session-start" && std::env::var_os("AMPUP_MEMORY_HOOK_DETACHED").is_none()
            {
                update::in_background(VERSION);
            }
            let r = match hook::Harness::parse(&harness) {
                Ok(_) if event == "outcome" => hook::outcome(session.as_deref(), &args).await,
                Ok(h) => hook::run_in(&event, h).await,
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                eprintln!("{} hook {event}: {e:#}", naming::CLIENT_BIN);
            }
            Ok(())
        }
        Cmd::Hooks {
            action,
            harness,
            url,
            org,
            dry_run,
        } => hook_install::cli(
            naming::CLIENT_BIN,
            &action,
            &harness,
            url.as_deref(),
            org.as_deref(),
            dry_run,
        ),
        Cmd::Mcp => hook::bridge().await,
        Cmd::Doctor => hook::doctor().await,
        Cmd::Metric {
            name,
            evidence,
            limit,
            filters,
            refresh,
        } => hook::metric(&name, evidence, limit, filters.as_deref(), refresh).await,
        Cmd::Update => {
            update::now(VERSION).await;
            Ok(())
        }
    }
}
