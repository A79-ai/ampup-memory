//! `memstore-client`: what a person's machine runs to reach a team's memory, without the store and server that
//! make memstore hundreds of MB. Its commands are memstore's own, at the same names: `join` to sign in, `hook` for
//! a coding agent's hooks, `mcp` for the server's MCP endpoint over stdio, `doctor` to check it all.

use anyhow::Result;
use clap::{Parser, Subcommand};
use memstore_client::{client, hook, update};

/// The release this was built as (`v0.1.0-rc.7`, from the release workflow), else the crate's version: the
/// plugin compares it with the latest release to keep the client current.
const VERSION: &str = match option_env!("MEMSTORE_RELEASE") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(
    name = "memstore-client",
    version = VERSION,
    about = "Reach a team's memstore from this machine"
)]
struct Cli {
    /// Use this context for one command (also MEMSTORE_CONTEXT)
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
    },
    /// A coding agent's hook: session-start, prompt, stop, session-end or status, reading the hook's JSON on stdin;
    /// `outcome good|bad [why]` rates the session's run. Never fails the session: errors go to stderr and it exits 0
    Hook {
        event: String,
        /// outcome: good or bad, then why
        args: Vec<String>,
        /// The agent it runs in: claude-code, codex, gemini, cursor or copilot
        #[arg(long, default_value = "claude-code")]
        harness: String,
        /// outcome: the session to rate (else CLAUDE_CODE_SESSION_ID, else the one last active in this repo)
        #[arg(long)]
        session: Option<String>,
    },
    /// The server's MCP endpoint over stdio, signed in as `join` left you. MEMSTORE_URL and MEMSTORE_TOKEN override
    Mcp,
    /// Whether this machine's memory works: the server, who is signed in, the org and how the last hook went
    Doctor,
    /// One of the memory's named metrics as JSON, answered as you see the memory (MEMSTORE_URL, MEMSTORE_TOKEN
    /// and MEMSTORE_ORG sign a service in)
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

#[tokio::main]
async fn main() -> Result<()> {
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
        } => {
            update::before_sign_in(VERSION).await;
            client::join(&server, org.as_deref(), team.as_deref(), no_browser, sso).await
        }
        Cmd::Hook {
            event,
            args,
            harness,
            session,
        } => {
            if event == "session-start" && std::env::var_os("MEMSTORE_HOOK_DETACHED").is_none() {
                update::in_background(VERSION);
            }
            let r = match hook::Harness::parse(&harness) {
                Ok(_) if event == "outcome" => hook::outcome(session.as_deref(), &args).await,
                Ok(h) => hook::run_in(&event, h).await,
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                eprintln!("memstore hook {event}: {e:#}");
            }
            Ok(())
        }
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
