//! The people side of the CLI: orgs, teams, invites, signing in, and which memory you're pointed at.
//!
//! ```text
//! memstore create-org acme --admin alice@acme.com     # and switch to it
//! memstore org init                                   # what acme wants from memory
//! memstore create-team sales
//! memstore invite --email bob@acme.com --team sales   # or --file people.csv
//! memstore join https://memory.acme.com --org acme --team sales   # what bob runs
//! memstore use acme/sales                             # switch context, like kubectl
//!
//! memstore auth setup --provider okta --issuer https://acme.okta.com --client-id 0oa1 --require
//! memstore auth domain add acme.com --team everyone     # verified acme.com emails join on their own
//! memstore auth map-group Sales --team sales            # the provider's groups decide teams
//! memstore apikey create --name ci --team sales         # a service account's key, shown once
//! ```
//!
//! Signed in to a server (a context with a server and credentials for it), these go to the server as you;
//! otherwise, or with `--direct`, they reach the catalog directly as its owner.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::Store;
use crate::people::client::{self, Config, Context, Credentials};
use crate::people::directory::{self, Actor, Change, Directory, Profile, Role, SyncMember};

#[derive(Subcommand)]
pub enum PeopleCmd {
    /// Create an org and switch to it (catalog owner or a platform admin key)
    CreateOrg {
        org: String,
        /// Its first admin (you, by default, when signed in as a person)
        #[arg(long)]
        admin: Option<String>,
        /// Don't switch the current context to it
        #[arg(long)]
        no_use: bool,
    },
    /// Create a team in the org
    CreateTeam {
        team: String,
        #[arg(long)]
        org: Option<String>,
    },
    /// Give people access: they become members of the org (and the team), and sign in with that email
    Invite(InviteArgs),
    /// Who asked to join the org (`memstore join` as someone the org doesn't hold yet) and waits for an admin:
    /// `memstore invite --email <email>` lets them in, `memstore decline` turns them away
    Requests {
        #[arg(long)]
        org: Option<String>,
    },
    /// Turn away someone who asked to join the org
    Decline {
        #[arg(long)]
        email: String,
        #[arg(long)]
        org: Option<String>,
    },
    /// Sign in to a server and add a context for an org (and team) there: what an invited person runs. Not a
    /// member of the org yet: asks to join it, and its admins are emailed to approve
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
    /// Set this machine up for a team's memory in one go: sign in (as `join`, unless already signed in to that
    /// org there), then install memstore in every coding agent found here (Claude Code's plugin; the hooks and
    /// the `memstore mcp` server for Codex, Gemini, Cursor and Copilot, signed in as the CLI is: no URL or token
    /// is written anywhere), then check it all (`memstore doctor`)
    Setup {
        server: String,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        team: Option<String>,
        /// Only these agents (claude-code, codex, gemini, cursor, copilot), comma-separated; else every one found
        #[arg(long, value_delimiter = ',')]
        harness: Vec<String>,
        #[arg(long)]
        no_browser: bool,
        /// Say what would change, and change nothing (no sign-in either)
        #[arg(long)]
        dry_run: bool,
    },
    /// Switch context: a context's name, or `<org>` / `<org>/<team>` on the current server
    Use { target: String },
    /// Who you are and which memory you're pointed at
    Whoami,
    /// Which caller-identity source this environment configures, and whether each part works
    #[command(subcommand)]
    Identity(IdentityCmd),
    /// Deprecated (admin keys aside): API keys in a keys file, read at server start. Use `apikey`
    #[command(subcommand)]
    Keys(KeysCmd),
    /// Disable or re-enable a user on a running server; forget a user (delete what they wrote)
    #[command(subcommand)]
    Users(UsersCmd),
    /// Change episodes' ACLs on a running server
    #[command(subcommand)]
    Acl(AclCmd),
    /// Orgs: init, list, show, delete, export, sync members from an IdP export
    #[command(subcommand)]
    Org(OrgCmd),
    /// Teams in an org (--org, else the context's org)
    #[command(subcommand)]
    Team(TeamCmd),
    /// An org's members and their roles
    #[command(subcommand)]
    Member(MemberCmd),
    /// Sign in to a server the way MCP clients do (OAuth with PKCE in the browser), or with an API key
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Service accounts: API keys for CI and other non-people, scoped to an org or one of its teams
    #[command(subcommand)]
    Apikey(ApikeyCmd),
    /// Contexts (server, org, team), like `kubectl config`
    #[command(subcommand)]
    Config(ConfigCmd),
    /// The selected memory on the server, as you
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// An org's audit log: access changes, sign-ins, refusals and key use (org admins); with
    /// --operator, the rows of no org: anonymous callers' and outsiders' refusals, and break-glass support
    /// sessions (the operator's admin key only)
    Audit {
        #[arg(long, conflicts_with = "operator")]
        org: Option<String>,
        /// The operator's view: refusals of anonymous callers and of outsiders, which belong to no org
        #[arg(long)]
        operator: bool,
        /// How far back: e.g. 24h, 7d, 30m
        #[arg(long, default_value = "24h")]
        since: String,
        /// Only change, signin, refusal, key, support or maintenance
        #[arg(long)]
        kind: Option<String>,
        /// One JSON object per event, one per line
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct InviteArgs {
    /// An email to invite (repeat for several)
    #[arg(long)]
    email: Vec<String>,
    /// A CSV of `email[,role[,team;team]]` (a header line is skipped), or the JSON `org sync` takes
    #[arg(long)]
    file: Option<PathBuf>,
    /// Also add everyone invited to this team
    #[arg(long)]
    team: Option<String>,
    #[arg(long)]
    org: Option<String>,
    /// admin or member
    #[arg(long, default_value = "member")]
    role: String,
}

#[derive(Subcommand)]
pub enum OrgCmd {
    /// Set up an org's memory: a few questions about what matters, then teams and next steps
    Init(InitArgs),
    /// Create an org (same as create-org, without switching)
    Create {
        org: String,
        #[arg(long)]
        admin: Option<String>,
    },
    /// Every org (direct or admin key), or yours (signed in)
    List,
    /// An org's teams, members and memory profile
    Show { org: Option<String> },
    /// The schema an org's memory is typed with (its `extract` section included), through the server; with
    /// --history every version committed on the branch, oldest first
    Schema {
        org: Option<String>,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        history: bool,
    },
    /// Type an org's memory with a built-in schema or a JSON file, through the server (an org admin); --dry-run
    /// makes every check the set would, the breaking-change check included, and commits nothing
    SetSchema {
        org: String,
        /// A built-in schema's name or a schema's JSON file
        schema: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete an org and all its memory, for good (the operator only): its keys and identity provider
    /// stop working, its members lose it, and its and its teams' tables, episode bodies and search
    /// indexes are deleted from storage. Run it again to finish a delete that stopped halfway
    Delete {
        org: String,
        /// The org's name again: there is no undo
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Write the org's memory (its own and each team's) to a directory: facts, entities and episodes
    /// as JSON lines, one directory per scope (org admins, or the operator). No keys or secrets
    Export {
        org: String,
        /// Where to write it (created if missing; files there with the same names are replaced)
        #[arg(long)]
        out: PathBuf,
    },
    /// Delete for good what was retracted in the org (episodes deleted at their source, POST /retract):
    /// their rows, bodies and the facts citing them, from its and its teams' tables and search indexes
    /// (org admins, or the operator). Readers already lost them at retraction; this removes them from
    /// storage. Run it now and then, or again to finish one that stopped halfway
    Purge { org: String },
    /// Show the org's (or a team's) retention policy, or set one: rules `<source prefix>=<age>`, the age in
    /// days, weeks or years or `forever`, the longest matching prefix winning (`slack:=90d
    /// slack:C-legal:=forever =2y`). Episodes older than their rule (by when they happened) are gone for
    /// every reader at once; `org purge` removes them from storage. Org admins, or the operator
    Retention {
        org: String,
        #[arg(long)]
        team: Option<String>,
        /// The new policy's rules (replacing the old ones)
        rules: Vec<String>,
        /// Keep everything: remove every rule
        #[arg(long, conflicts_with = "rules")]
        clear: bool,
    },
    /// Show the org's settings, or set them (the org's, or with --team one team's): what it compiles skills
    /// from, whether it learns advice, serves a note with its procedures, the evidence a note needs before it is
    /// served and whether it extracts goal-blind (org admins), and the models it runs on, on the server's backends
    /// (the operator only, as a model spends the platform's budget; "" puts one back to the server's)
    Settings {
        org: String,
        #[arg(long)]
        team: Option<String>,
        /// Compile the skill library where compile sources are set (default true; false turns it off)
        #[arg(long)]
        compile: Option<bool>,
        /// The sources the skill library is compiled from: source prefixes (kb:,github:) or * for every
        /// source ("" unsets them)
        #[arg(long)]
        compile_sources: Option<String>,
        /// Sources per compile round (default: the server's MEMSTORE_COMPILE_BATCH, 12; 0 unsets it)
        #[arg(long)]
        compile_batch: Option<usize>,
        /// What the compile sources are: errors (error reports: each skill's when names the exception and its
        /// message) ("" unsets it)
        #[arg(long)]
        compile_kind: Option<String>,
        /// Learn advice from rewarded runs and judged sessions on its own (default true; false turns it off)
        #[arg(long)]
        advice: Option<bool>,
        /// Serve a situation's learned note with its linked procedures (default false: the note alone)
        #[arg(long)]
        advice_procedures: Option<bool>,
        /// The people a learned note must have helped before it is served outside the trial arm (default: the
        /// server's MEMSTORE_ADVICE_EVIDENCE, else 3; 0 serves every note)
        #[arg(long)]
        advice_evidence: Option<usize>,
        /// The people a team note learned from runs, and each of its items, must come from (default: the server's
        /// MEMSTORE_ADVICE_LEARN_PEOPLE, else 3; never below 2)
        #[arg(long)]
        advice_learn_people: Option<usize>,
        /// How advice routes over the skills compiled from documents: fallback (default: only where no kind of
        /// task learned from runs fits), together (both on one list) or off ("" unsets it)
        #[arg(long)]
        advice_skills: Option<String>,
        /// Accounts whose runs and sessions teach the team's notes nothing (a shared login, an eval harness's, a
        /// bot's), comma-separated user ids; they still teach their own notes. "" unsets it
        #[arg(long)]
        advice_learn_exclude: Option<String>,
        /// Lead advice asked about an error to similar past errors and the PRs that fixed them, from the sentry: and
        /// github: episodes the caller reads (default false)
        #[arg(long)]
        error_leads: Option<bool>,
        /// Extract episodes no goal reads, goal-blind, so conversations reach search (default true; false turns it off)
        #[arg(long)]
        extract: Option<bool>,
        /// The model facts are extracted with (e.g. anthropic/claude-haiku-5.5)
        #[arg(long)]
        extract_model: Option<String>,
        /// The model skill libraries are compiled with (e.g. qwen/qwen3.7-plus)
        #[arg(long)]
        compile_model: Option<String>,
        /// The model each skill's AIP graph is compiled with (e.g. anthropic/claude-haiku-5.5)
        #[arg(long)]
        aip_model: Option<String>,
        /// Keep the scope's model calls whole in the server's training log (MEMSTORE_TRAIN_LOG), to train the org's
        /// own models from (the operator only; default false)
        #[arg(long)]
        train_log: Option<bool>,
        /// Have the plugin's hooks upload the transcripts of sessions that called the advisor or get_advice, to train an advice
        /// model from (the operator only; default false)
        #[arg(long)]
        advice_traces: Option<bool>,
        /// Rank fact search the earlier way: reciprocal-rank fusion with recency, no groups (default
        /// false)
        #[arg(long)]
        classic_ranking: Option<bool>,
        /// Mark what the schema says has gone stale on read: values past their predicate's freshness_days, and
        /// plans whose date has passed while still pending (default: the server's MEMSTORE_FRESHNESS, off)
        #[arg(long)]
        freshness: Option<bool>,
        /// Curate learned advice: withhold a note whose served runs do worse than the rest, change notes by small
        /// edits, merge situations that are the same task (default false, or the server's MEMSTORE_ADVICE_CURATION)
        #[arg(long)]
        advice_curation: Option<bool>,
        /// Route advice for sessions a program runs (a harness named `:sdk`: `claude -p`, an SDK run) (default
        /// false: advice is learned from people's sessions only, so a program's kind of task never has a note)
        #[arg(long)]
        advice_automated: Option<bool>,
        /// Fold each person's own notes into one deduplicated, capped profile that start_session hands them with
        /// their newest few notes, instead of their newest 20 alone (default: the server's MEMSTORE_PROFILE, off)
        #[arg(long)]
        profile: Option<bool>,
        /// Other scopes of the org its readers also search, comma-separated (team:<org>/<team>; "" unsets them;
        /// the operator only)
        #[arg(long)]
        reads: Option<String>,
        /// Delete from storage in the nightly maintenance window what the org's sources deleted and what aged past
        /// its retention, once hidden for MEMSTORE_PURGE_GRACE (7 days) (default false)
        #[arg(long)]
        purge: Option<bool>,
        /// Whether the org's admins may read each person's activity: their questions, answers and what each
        /// used (default true)
        #[arg(long)]
        activity: Option<bool>,
        /// Whether each session that writes gets a branch of its own, merged into main once it ends (default
        /// false)
        #[arg(long)]
        session_branches: Option<bool>,
        /// Whether a change that waits for an entity's owners is decided by a decisions model instead, each decision
        /// recorded for the owners to review, who may still approve what it rejected (default true)
        #[arg(long)]
        auto_review: Option<bool>,
        /// Replace payment card numbers, US social security numbers, IBANs and international phone numbers in text
        /// written from now on with [card], [ssn], [iban] and [phone] before it is stored (default false)
        #[arg(long)]
        pii: Option<bool>,
    },
    /// Bind the org to an Auth0 Organization (the operator only): a sign-in through the platform's Auth0
    /// tenant counts in the org only from that Organization (for example, the host application's org id)
    BindAuth0 {
        org: String,
        /// The Organization's id, org_…
        auth0_org: String,
    },
    /// Make the org's members, roles and teams match an IdP export (JSON or CSV `user,role,team;team`)
    Sync {
        file: PathBuf,
        #[arg(long)]
        org: Option<String>,
        /// Keep members the file doesn't list
        #[arg(long)]
        keep_absent: bool,
        /// Show the changes without making them
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args)]
pub struct InitArgs {
    #[arg(long)]
    org: Option<String>,
    /// What the org does, in a sentence
    #[arg(long)]
    description: Option<String>,
    /// sales, engineering or general (general: no fixed types)
    #[arg(long)]
    kind: Option<String>,
    /// What memory should pay most attention to (`;`-separated)
    #[arg(long)]
    focus: Option<String>,
    /// What memory must never keep (`;`-separated)
    #[arg(long)]
    avoid: Option<String>,
    /// Where knowledge lives: slack, email, calls, docs, github, crm (comma-separated)
    #[arg(long)]
    sources: Option<String>,
    /// Teams to create now (comma-separated)
    #[arg(long)]
    teams: Option<String>,
    /// Take the suggested answer to every question not given as a flag
    #[arg(long, short)]
    yes: bool,
}

#[derive(Subcommand)]
pub enum TeamCmd {
    /// Create a team (same as create-team)
    Create {
        team: String,
        #[arg(long)]
        org: Option<String>,
    },
    Delete {
        team: String,
        #[arg(long)]
        org: Option<String>,
    },
    /// Teams and their members
    List {
        #[arg(long)]
        org: Option<String>,
    },
    /// Add members to a team (and to its org if they aren't yet)
    Add {
        team: String,
        #[arg(required = true, num_args = 1..)]
        users: Vec<String>,
        #[arg(long)]
        org: Option<String>,
    },
    Remove {
        team: String,
        #[arg(required = true, num_args = 1..)]
        users: Vec<String>,
        #[arg(long)]
        org: Option<String>,
    },
    /// Pilot from a csv (`email,name,manager,role,team`), no IdP: print each person's closure
    Check { csv: PathBuf },
    /// Pilot: issue one API key per person into the keys file (`MEMSTORE_API_KEYS`), printing each once
    Keys {
        csv: PathBuf,
        /// JSON rows; created if missing
        keys_file: PathBuf,
        /// Only these people (comma-separated emails); otherwise everyone without a key
        #[arg(long)]
        only: Option<String>,
        /// Re-issue for everyone (old keys stop working)
        #[arg(long)]
        rotate: bool,
    },
    /// Pilot: replace a running server's team: POST the csv to its `/team` with an admin key
    Load {
        csv: PathBuf,
        /// The server, e.g. http://127.0.0.1:8080
        url: String,
        /// An admin API key (or MEMSTORE_ADMIN_KEY)
        #[arg(long, env = "MEMSTORE_ADMIN_KEY")]
        key: String,
    },
}

#[derive(Subcommand)]
pub enum MemberCmd {
    /// Add a member, or change their role
    Add {
        user: String,
        #[arg(long, default_value = "member")]
        role: String,
        #[arg(long)]
        org: Option<String>,
    },
    Remove {
        user: String,
        #[arg(long)]
        org: Option<String>,
    },
    List {
        #[arg(long)]
        org: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum IdentityCmd {
    /// What `serve` would make of this environment's caller identity, each part tried (files read,
    /// JWKS fetched, the provider discovered, Cloud Run asked); exits non-zero if any part fails
    Check,
}

#[derive(Subcommand)]
pub enum KeysCmd {
    /// Issue an API key into the keys file (`MEMSTORE_API_KEYS`, created if missing), printing it once
    New {
        keys_file: PathBuf,
        /// The user the key acts as (an email, or `svc:<name>` for a service)
        #[arg(long)]
        user: String,
        /// Groups the key holds, comma-separated
        #[arg(long)]
        groups: Option<String>,
        /// May change the directory, closures and ACLs
        #[arg(long)]
        admin: bool,
        /// The row's id (default: the user)
        #[arg(long)]
        id: Option<String>,
    },
    /// Remove a key by its id (takes effect when the server next starts)
    Revoke { keys_file: PathBuf, id: String },
}

#[derive(Subcommand)]
pub enum UsersCmd {
    /// Refuse every request as this user from now on (a deprovisioned account)
    Disable {
        /// The server, e.g. http://127.0.0.1:8080
        url: String,
        user: String,
        /// An admin API key (or MEMSTORE_ADMIN_KEY)
        #[arg(long, env = "MEMSTORE_ADMIN_KEY", hide_env_values = true)]
        key: String,
    },
    /// Let a disabled user back in
    Enable {
        url: String,
        user: String,
        #[arg(long, env = "MEMSTORE_ADMIN_KEY", hide_env_values = true)]
        key: String,
    },
    /// Forget a person: remove them from the org and its teams, and delete what they wrote there
    /// (their remember_memory episodes, with the facts citing them) from storage, leaving a tombstone in
    /// the directory. An org's admins forget in their own org; the operator in any, or every org when
    /// --org is left out
    Forget {
        user: String,
        #[arg(long)]
        org: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AclCmd {
    /// Change who may read some committed episodes (the source's change feed)
    Set {
        /// The server, e.g. http://127.0.0.1:8080
        url: String,
        /// The new ACL: `*`, `user:<id>`, `group:<id>`, `role:<name>` (comma-separated)
        acl: String,
        /// Every episode whose source starts with this
        #[arg(long, conflicts_with_all = ["source", "episode"])]
        source_prefix: Option<String>,
        /// Every episode of exactly this source
        #[arg(long, conflicts_with = "episode")]
        source: Option<String>,
        /// One episode
        #[arg(long)]
        episode: Option<String>,
        #[arg(long, env = "MEMSTORE_ADMIN_KEY", hide_env_values = true)]
        key: String,
    },
}

#[derive(Subcommand)]
pub enum AuthCmd {
    /// Sign in to the context's server (or --server); the server says which identity provider to use
    Login {
        #[arg(long)]
        server: Option<String>,
        /// Sign in with an API key instead (service callers, CI, a platform admin)
        #[arg(long, env = "MEMSTORE_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// The identity provider, when the server doesn't publish it (e.g. https://accounts.google.com)
        #[arg(long)]
        issuer: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        /// Only for providers that give public (installed-app) clients one, like Google
        #[arg(long, env = "MEMSTORE_OIDC_CLIENT_SECRET", hide_env_values = true)]
        client_secret: Option<String>,
        /// Print the sign-in URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
        /// The loopback port the provider redirects to (http://127.0.0.1:<port>/callback), for providers that
        /// only take registered redirect URIs; else MEMSTORE_LOGIN_PORT, the server's, or any free port
        #[arg(long)]
        port: Option<u16>,
    },
    /// Forget the sign-in for the context's server
    Logout,
    /// Who is signed in, and what the server says they belong to
    Status,
    /// Print a current bearer token (renewed if needed), for curl or an MCP client's header
    PrintToken,
    /// Connect the org's own identity provider (org admins), and/or try signing in through it (--test)
    Setup(SetupArgs),
    /// Go back to signing in through the server's identity provider
    RemoveProvider {
        #[arg(long)]
        org: Option<String>,
    },
    /// Email domains whose verified addresses join the org without an invite
    #[command(subcommand)]
    Domain(DomainCmd),
    /// People the org's provider puts in <group> are members of --team, for as long as it does
    MapGroup {
        group: String,
        #[arg(long)]
        team: String,
        #[arg(long)]
        org: Option<String>,
    },
    UnmapGroup {
        group: String,
        #[arg(long)]
        team: String,
        #[arg(long)]
        org: Option<String>,
    },
}

#[derive(Args)]
pub struct SetupArgs {
    #[arg(long)]
    pub org: Option<String>,
    /// google, okta, entra or oidc (any OpenID Connect provider)
    #[arg(long)]
    pub provider: Option<String>,
    /// The provider's issuer URL (okta: https://<you>.okta.com; oidc: any; entra: or --tenant)
    #[arg(long)]
    pub issuer: Option<String>,
    /// Entra ID: the directory (tenant) id
    #[arg(long)]
    pub tenant: Option<String>,
    /// The client people sign in with (repeat for more: Claude's, Cursor's, ...); the first is the CLI's
    #[arg(long = "client-id")]
    pub client_ids: Vec<String>,
    /// The CLI client (the first --client-id) is public: an installed app, signing in with PKCE and no
    /// secret, or a secret the provider doesn't treat as confidential (Google's installed apps). Its
    /// secret is handed to anyone who asks how to sign in
    #[arg(long, conflicts_with = "confidential_client")]
    pub public_client: bool,
    /// The CLI client is confidential (a web app): its secret is never taken, and the CLI signs in with
    /// PKCE alone. If the provider won't allow that, register a public client for the CLI instead
    #[arg(long)]
    pub confidential_client: bool,
    /// A public client's secret, for providers that give installed apps one (Google): the secret (the
    /// server seals it with its MEMSTORE_SECRET_KEY) or env:MEMSTORE_IDP_SECRET_<NAME> (that server
    /// variable). Needs --public-client
    #[arg(long)]
    pub client_secret: Option<String>,
    /// The ID-token claim that lists groups
    #[arg(long, default_value = "groups")]
    pub groups_claim: String,
    /// Members must sign in through this provider
    #[arg(long)]
    pub require: bool,
    /// Sign in through the org's provider and show what it says about you
    #[arg(long)]
    pub test: bool,
    #[arg(long)]
    pub no_browser: bool,
}

#[derive(Subcommand)]
pub enum DomainCmd {
    /// Verified emails at <domain> join the org (and --team); org admins can add their own email's domain
    Add {
        domain: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        org: Option<String>,
    },
    Remove {
        domain: String,
        #[arg(long)]
        org: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ApikeyCmd {
    /// Make a key (shown once; the server keeps only its sha256)
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        org: Option<String>,
        /// Read (and write) only this team's memory; otherwise the org's own
        #[arg(long)]
        team: Option<String>,
        /// reader; writer (may also add memory); ingest (only adds memory, reads nothing); directory
        /// (a SCIM bridge's: adds and removes the org's non-admin members and manages its teams through
        /// /admin, touches no memory; org-wide, so no --team); or recorder (a CI job's: records runs and
        /// asks advice as a member it names in x-memstore-as, does nothing as itself)
        #[arg(long, default_value = "reader")]
        role: String,
        /// How long it works: e.g. 90d, 12h, 30m, 45s (default: until revoked)
        #[arg(long)]
        expires: Option<String>,
        /// An ingest key's source prefixes, comma-separated (e.g. email:,slack:acme:): it writes and
        /// retracts only sources starting with one of them (default: any source)
        #[arg(long)]
        sources: Option<String>,
    },
    /// Hold an ingest key to source prefixes, or (--any) let it write any source again
    Sources {
        name: String,
        #[arg(long)]
        org: Option<String>,
        /// Comma-separated prefixes (e.g. email:,ampup:meeting:,slack:acme:)
        #[arg(required_unless_present = "any", conflicts_with = "any")]
        sources: Option<String>,
        /// Drop the key's prefixes: it writes any source it may
        #[arg(long)]
        any: bool,
    },
    /// The org's keys, without their secrets
    List {
        #[arg(long)]
        org: Option<String>,
    },
    /// Stop a key working, from its next request
    Revoke {
        name: String,
        #[arg(long)]
        org: Option<String>,
    },
    /// Move file keys (`MEMSTORE_API_KEYS`, `memstore keys new`) into the directory as the org's (or
    /// --team's) keys: same key, same holder, no more rights. The operator imports from the keys file
    /// (--from, through a server that has it, or --direct); a key's holder imports their own (--self,
    /// signed in with it). Admin keys and keys holding groups are refused; only sha256s are read
    Import {
        #[arg(long)]
        org: Option<String>,
        /// Each key reads (and writes) only this team's memory; its user must be in the team
        #[arg(long)]
        team: Option<String>,
        /// The keys file: JSON rows of {id, sha256, user, groups, admin} (the operator)
        #[arg(
            long,
            required_unless_present = "self_import",
            conflicts_with = "self_import"
        )]
        from: Option<PathBuf>,
        /// The file key you are signed in with (its holder)
        #[arg(long = "self")]
        self_import: bool,
        /// Only these rows (comma-separated ids)
        #[arg(long, requires = "from")]
        only: Option<String>,
        /// writer (what a file key could do), or narrower: reader, ingest
        #[arg(long, default_value = "writer")]
        role: String,
        /// Show what would be imported, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
}

/// `90d`, `12h`, `30m` or `45s`, in seconds.
fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().ok().filter(|n| *n > 0).with_context(|| {
        format!("--expires {s:?}: a number and a unit, like 90d, 12h, 30m or 45s")
    })?;
    let per = match unit {
        "d" => 86_400,
        "h" => 3_600,
        "m" => 60,
        "s" => 1,
        _ => anyhow::bail!("--expires {s:?}: the unit is d, h, m or s"),
    };
    Ok(n * per)
}

impl AuthCmd {
    /// Changes the directory (rather than this machine's sign-in).
    fn changes_directory(&self) -> bool {
        match self {
            AuthCmd::Setup(a) => a.provider.is_some(),
            AuthCmd::RemoveProvider { .. }
            | AuthCmd::Domain(_)
            | AuthCmd::MapGroup { .. }
            | AuthCmd::UnmapGroup { .. } => true,
            _ => false,
        }
    }
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Every context; `*` marks the current one
    GetContexts,
    CurrentContext,
    UseContext {
        name: String,
    },
    /// Create or change a context (and switch to it with --use)
    SetContext {
        name: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long = "use")]
        use_it: bool,
    },
    DeleteContext {
        name: String,
    },
    /// The whole config file
    View,
    /// Set server, org, team or user on the current context
    Set {
        key: String,
        value: String,
    },
    Get {
        key: String,
    },
    Unset {
        key: String,
    },
}

#[derive(Subcommand)]
pub enum RemoteCmd {
    /// Who the server says you are, the memory you selected, and your orgs and teams
    Me,
    /// Text search over the selected memory's facts
    Search {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        #[arg(long, default_value_t = 10)]
        k: usize,
    },
    /// A typed query (JSON, or @file) on the selected memory
    Query { query: String },
}

/// Whether `cmd` needs the catalog: directory commands do when they don't go to a server.
pub fn needs_catalog(cmd: &PeopleCmd, direct: bool) -> Result<bool> {
    let directory = matches!(
        cmd,
        PeopleCmd::CreateOrg { .. }
            | PeopleCmd::CreateTeam { .. }
            | PeopleCmd::Invite(_)
            | PeopleCmd::Requests { .. }
            | PeopleCmd::Decline { .. }
            | PeopleCmd::Org(_)
            | PeopleCmd::Team(
                TeamCmd::Create { .. }
                    | TeamCmd::Delete { .. }
                    | TeamCmd::List { .. }
                    | TeamCmd::Add { .. }
                    | TeamCmd::Remove { .. }
            )
            | PeopleCmd::Member(_)
            | PeopleCmd::Apikey(_)
            | PeopleCmd::Audit { .. }
            | PeopleCmd::Users(UsersCmd::Forget { .. })
    ) || matches!(cmd, PeopleCmd::Auth(a) if a.changes_directory());
    Ok(directory && (direct || !signed_in()?))
}

/// The context has a server and a sign-in for it.
fn signed_in() -> Result<bool> {
    Ok(Credentials::load(None)?.is_some())
}

/// Run `cmd`; `store` is the catalog when [`needs_catalog`] said so.
pub async fn run(cmd: &PeopleCmd, store: Option<&Store>) -> Result<()> {
    let to = Target { store };
    match cmd {
        PeopleCmd::CreateOrg { org, admin, no_use } => {
            let admin = match admin {
                Some(a) => Some(a.clone()),
                // Signed in as a person: the creator is its first admin.
                None if store.is_none() => Credentials::load(None)?
                    .filter(|c| c.api_key.is_none())
                    .map(|c| c.user),
                None => None,
            };
            to.change(
                Change::CreateOrg {
                    org: org.clone(),
                    admin,
                    auth0_org: None,
                },
                false,
            )
            .await?;
            if !no_use {
                let mut config = Config::load()?;
                let mut ctx = config.context();
                ctx.set("org", Some(org))?;
                config.put_context(org, ctx);
                config.save()?;
                println!("Switched to context {org}. Next: memstore org init");
            }
        }
        PeopleCmd::CreateTeam { team, org } | PeopleCmd::Team(TeamCmd::Create { team, org }) => {
            let org = org_of(org)?;
            to.change(
                Change::CreateTeam {
                    org: org.clone(),
                    team: team.clone(),
                },
                false,
            )
            .await?;
            println!(
                "Next: memstore invite --email <email> --team {team}; memstore use {org}/{team}"
            );
        }
        PeopleCmd::Invite(a) => invite(&to, a).await?,
        PeopleCmd::Requests { org } => {
            let org = org_of(org)?;
            let view = to.view(Some(&org)).await?;
            let pending = view["pending"].as_object().cloned().unwrap_or_default();
            if pending.is_empty() {
                println!("nobody is waiting to join {org}");
            } else {
                for (user, at) in &pending {
                    let when = at
                        .as_i64()
                        .and_then(chrono::DateTime::from_timestamp_micros)
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_default();
                    println!("{user}  asked {when}");
                }
                println!(
                    "\nmemstore invite --org {org} --email <email> lets them in; memstore decline --org {org} --email <email> turns them away"
                );
            }
        }
        PeopleCmd::Decline { email, org } => {
            let org = org_of(org)?;
            to.change(
                Change::Decline {
                    org,
                    user: email.clone(),
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Join {
            server,
            org,
            team,
            no_browser,
            sso,
        } => join(server, org.as_deref(), team.as_deref(), *no_browser, *sso).await?,
        PeopleCmd::Setup {
            server,
            org,
            team,
            harness,
            no_browser,
            dry_run,
        } => {
            set_up_machine(
                server,
                org.as_deref(),
                team.as_deref(),
                harness,
                *no_browser,
                *dry_run,
            )
            .await?
        }
        PeopleCmd::Use { target } => {
            let mut config = Config::load()?;
            if config.contexts.contains_key(target) {
                config.use_context(target)?;
            } else {
                let (org, team) = match target.split_once('/') {
                    Some((o, t)) => (o, Some(t)),
                    None => (target.as_str(), None),
                };
                let mut ctx = config.context();
                ctx.set("org", Some(org))?;
                ctx.set("team", team)?;
                config.put_context(target, ctx);
            }
            config.save()?;
            let ctx = config.context();
            println!(
                "Switched to context {target} ({}{}{})",
                ctx.org.as_deref().unwrap_or("no org"),
                ctx.team
                    .as_deref()
                    .map(|t| format!("/{t}"))
                    .unwrap_or_default(),
                ctx.server
                    .as_deref()
                    .map(|s| format!(" on {s}"))
                    .unwrap_or_default()
            );
        }
        PeopleCmd::Whoami | PeopleCmd::Remote(RemoteCmd::Me) => {
            print_json(&client::call("GET", "/me", None).await?)?
        }
        PeopleCmd::Remote(r) => remote(r).await?,
        PeopleCmd::Org(OrgCmd::Init(a)) => init(&to, a).await?,
        PeopleCmd::Org(OrgCmd::Create { org, admin }) => {
            to.change(
                Change::CreateOrg {
                    org: org.clone(),
                    admin: admin.clone(),
                    auth0_org: None,
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Org(OrgCmd::Delete { org, confirm }) => {
            let change = Change::DeleteOrg {
                org: org.clone(),
                confirm: confirm.clone(),
            };
            to.change(change, false).await?;
        }
        PeopleCmd::Org(OrgCmd::Export { org, out }) => export(&to, org, out).await?,
        PeopleCmd::Org(OrgCmd::Retention {
            org,
            team,
            rules,
            clear,
        }) => {
            let rules = match (*clear, rules.is_empty()) {
                (true, _) => Some(vec![]),
                (false, true) => None,
                (false, false) => {
                    // Checked here too, so a typo says what is wrong.
                    crate::retention::Retention::parse(rules)?;
                    Some(rules.clone())
                }
            };
            let v = match to.store {
                Some(store) => {
                    crate::offboard::retention(
                        store,
                        &owner(),
                        org,
                        team.as_deref(),
                        rules.as_deref(),
                    )
                    .await?
                }
                None => {
                    let body = json!({"op": "retention", "org": org, "team": team, "rules": rules});
                    client::call("POST", "/admin", Some(body)).await?
                }
            };
            print_json(&v)?;
        }
        PeopleCmd::Org(OrgCmd::Purge { org }) => {
            let v = match to.store {
                Some(store) => crate::offboard::purge_retracted(store, &owner(), org).await?,
                None => {
                    let body = json!({"op": "purge_retracted", "org": org});
                    client::call("POST", "/admin", Some(body)).await?
                }
            };
            print_json(&v)?;
        }
        PeopleCmd::Users(UsersCmd::Forget { user, org }) => {
            let v = match to.store {
                Some(store) => {
                    crate::offboard::forget_user(store, &owner(), user, org.as_deref()).await?
                }
                None => {
                    let body = json!({"op": "forget_user", "user": user, "org": org});
                    client::call("POST", "/admin", Some(body)).await?
                }
            };
            for c in strings(&v["changes"]) {
                println!("{c}");
            }
        }
        PeopleCmd::Org(OrgCmd::Sync {
            file,
            org,
            keep_absent,
            dry_run,
        }) => {
            let members = directory::parse_members(&std::fs::read_to_string(file)?)?;
            let change = Change::Sync {
                org: org_of(org)?,
                members,
                keep_absent: *keep_absent,
            };
            to.change(change, *dry_run).await?;
        }
        PeopleCmd::Org(OrgCmd::List) => print_json(&to.view(None).await?)?,
        PeopleCmd::Org(OrgCmd::Show { org }) => print_json(&to.view(Some(&org_of(org)?)).await?)?,
        PeopleCmd::Org(OrgCmd::SetSchema {
            org,
            schema,
            dry_run,
        }) => to.put_schema(org, schema, *dry_run).await?,
        PeopleCmd::Org(OrgCmd::Schema {
            org,
            branch,
            history,
        }) => print_json(&to.schema_of(&org_of(org)?, branch, *history).await?)?,
        PeopleCmd::Org(OrgCmd::Settings {
            org,
            team,
            compile,
            compile_sources,
            compile_batch,
            compile_kind,
            advice,
            advice_procedures,
            advice_evidence,
            advice_learn_people,
            advice_skills,
            advice_learn_exclude,
            error_leads,
            extract,
            extract_model,
            compile_model,
            aip_model,
            train_log,
            advice_traces,
            classic_ranking,
            freshness,
            advice_curation,
            advice_automated,
            profile,
            reads,
            purge,
            activity,
            session_branches,
            auto_review,
            pii,
        }) => {
            let settings = directory::Settings {
                compile: *compile,
                compile_sources: compile_sources
                    .as_deref()
                    .map(crate::compile::parse_sources),
                compile_batch: *compile_batch,
                compile_kind: compile_kind.clone(),
                advice: *advice,
                advice_procedures: *advice_procedures,
                advice_evidence: *advice_evidence,
                advice_learn_people: *advice_learn_people,
                advice_skills: advice_skills.clone(),
                advice_learn_exclude: advice_learn_exclude.as_deref().map(|r| {
                    r.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                }),
                error_leads: *error_leads,
                extract: *extract,
                extract_model: extract_model.clone(),
                compile_model: compile_model.clone(),
                aip_model: aip_model.clone(),
                train_log: *train_log,
                advice_traces: *advice_traces,
                classic_ranking: *classic_ranking,
                freshness: *freshness,
                advice_curation: *advice_curation,
                advice_automated: *advice_automated,
                profile: *profile,
                reads: reads.as_deref().map(|r| {
                    r.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                }),
                purge: *purge,
                activity: *activity,
                session_branches: *session_branches,
                auto_review: *auto_review,
                pii: *pii,
            };
            if settings.is_empty() {
                let view = to.view(Some(org)).await?;
                let v = match team {
                    Some(t) => &view["team_settings"][t],
                    None => &view["settings"],
                };
                print_json(v)?;
            } else {
                to.change(
                    Change::SetSettings {
                        org: org.clone(),
                        team: team.clone(),
                        settings,
                    },
                    false,
                )
                .await?;
            }
        }
        PeopleCmd::Org(OrgCmd::BindAuth0 { org, auth0_org }) => {
            to.change(
                Change::SetAuth0Org {
                    org: org.clone(),
                    auth0_org: Some(auth0_org.clone()),
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Team(TeamCmd::List { org }) => {
            let view = to.view(Some(&org_of(org)?)).await?;
            for (t, members) in view["teams"].as_object().into_iter().flatten() {
                let m: Vec<&str> = members
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| u.as_str())
                    .collect();
                println!("{t}\t{}", m.join(", "));
            }
        }
        PeopleCmd::Member(MemberCmd::List { org }) => {
            let view = to.view(Some(&org_of(org)?)).await?;
            for (u, role) in view["members"].as_object().into_iter().flatten() {
                println!("{u}\t{}", role.as_str().unwrap_or_default());
            }
        }
        PeopleCmd::Team(TeamCmd::Delete { team, org }) => {
            to.change(
                Change::DeleteTeam {
                    org: org_of(org)?,
                    team: team.clone(),
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Team(TeamCmd::Add { team, users, org }) => {
            to.change(
                Change::AddToTeam {
                    org: org_of(org)?,
                    team: team.clone(),
                    users: users.clone(),
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Team(TeamCmd::Remove { team, users, org }) => {
            to.change(
                Change::RemoveFromTeam {
                    org: org_of(org)?,
                    team: team.clone(),
                    users: users.clone(),
                },
                false,
            )
            .await?;
        }
        PeopleCmd::Member(MemberCmd::Add { user, role, org }) => {
            let change = Change::AddMember {
                org: org_of(org)?,
                user: user.clone(),
                role: Role::parse(role)?,
            };
            to.change(change, false).await?;
        }
        PeopleCmd::Member(MemberCmd::Remove { user, org }) => {
            let change = Change::RemoveMember {
                org: org_of(org)?,
                user: user.clone(),
            };
            to.change(change, false).await?;
        }
        PeopleCmd::Auth(a) => auth(&to, a).await?,
        PeopleCmd::Team(
            t @ (TeamCmd::Check { .. } | TeamCmd::Keys { .. } | TeamCmd::Load { .. }),
        ) => pilot(t).await?,
        PeopleCmd::Config(c) => config(c)?,
        PeopleCmd::Identity(IdentityCmd::Check) => identity_check().await?,
        PeopleCmd::Keys(k) => keys(k)?,
        PeopleCmd::Users(u) => users(u).await?,
        PeopleCmd::Acl(a) => acl(a).await?,
        PeopleCmd::Apikey(k) => apikey(&to, k).await?,
        PeopleCmd::Audit {
            org,
            operator,
            since,
            kind,
            json,
        } => {
            let org = match operator {
                true => None,
                false => Some(org_of(org)?),
            };
            audit(&to, org.as_deref(), since, kind.as_deref(), *json).await?
        }
    }
    Ok(())
}

/// `memstore apikey`: service accounts' keys. The key is made here and shown once; the directory gets
/// only its sha256, so neither the server nor the catalog ever holds it.
async fn apikey(to: &Target<'_>, cmd: &ApikeyCmd) -> Result<()> {
    match cmd {
        ApikeyCmd::Create {
            name,
            org,
            team,
            role,
            expires,
            sources,
        } => {
            let org = org_of(org)?;
            let role = directory::KeyRole::parse(role)?;
            let expires_at = match expires {
                Some(e) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64;
                    Some(now + parse_duration(e)?)
                }
                None => None,
            };
            // 256 random bits.
            let key = format!(
                "{}{}{}",
                crate::auth::KEY_PREFIX,
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            // Sent once (over TLS to a server, which hashes it); the directory keeps only its sha256.
            let change = Change::CreateKey {
                org: org.clone(),
                name: name.clone(),
                team: team.clone(),
                role,
                expires_at,
                key: directory::NewKey(key.clone()),
                sources: sources
                    .as_deref()
                    .map(crate::split_list)
                    .unwrap_or_default(),
            };
            to.change(change, false).await?;
            let server = Config::load()?
                .get("server")
                .unwrap_or_else(|| "<server url>".into());
            let query = format!(
                "org={org}{}",
                team.as_deref()
                    .map(|t| format!("&team={t}"))
                    .unwrap_or_default()
            );
            println!("\nThe key (shown once; keep it in your CI's secrets):\n  {key}");
            if role == directory::KeyRole::Directory {
                println!(
                    "\nA directory key manages org {org}'s members and teams (never its admins) through /admin, and reads no memory:"
                );
                println!(
                    "  curl -X POST {server}/admin -H 'Authorization: Bearer <key>' -H 'content-type: application/json' \\\n    -d '{{\"op\": \"add_member\", \"org\": \"{org}\", \"user\": \"ada@example.com\"}}'"
                );
                return Ok(());
            }
            if role == directory::KeyRole::Recorder {
                println!(
                    "\nA recorder key records runs, rewards them and asks advice as a member of {org}{} it names, and does nothing as itself:",
                    team.as_deref().map(|t| format!("/{t}")).unwrap_or_default()
                );
                println!(
                    "  curl -X POST {server}/advice -H 'Authorization: Bearer <key>' -H 'x-memstore-as: ada@example.com' \\\n    -H 'content-type: application/json' -d '{{\"question\": \"...\"}}'"
                );
                return Ok(());
            }
            println!("\nUse it:");
            println!("  memstore auth login --server {server} --api-key {key}");
            println!(
                "  claude mcp add --transport http memstore '{server}/mcp?{query}' --header 'Authorization: Bearer {key}'"
            );
        }
        ApikeyCmd::List { org } => {
            let view = to.view(Some(&org_of(org)?)).await?;
            let keys = view["keys"].as_object().cloned().unwrap_or_default();
            if keys.is_empty() {
                println!("no keys");
            }
            for (name, k) in &keys {
                let s = |f: &str| k[f].as_str().unwrap_or("-").to_string();
                // Last, and only for a key held to sources: the columns before it stay as they were.
                let sources = k["sources"].as_array().map_or(String::new(), |a| {
                    let a: Vec<&str> = a.iter().filter_map(|v| v.as_str()).collect();
                    format!("\tsources {}", a.join(","))
                });
                println!(
                    "{name}\t{}\t{}\texpires {}\tcreated by {} at {}\tlast used {}{sources}",
                    k["team"].as_str().unwrap_or("(org)"),
                    s("role"),
                    k["expires_at"]
                        .as_i64()
                        .map_or("never".into(), |t| t.to_string()),
                    s("created_by"),
                    k["created_at"].as_i64().unwrap_or_default(),
                    k["last_used_at"]
                        .as_i64()
                        .and_then(chrono::DateTime::from_timestamp_micros)
                        .map_or("never".into(), |t| t
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                );
            }
        }
        ApikeyCmd::Import {
            org,
            team,
            from,
            self_import,
            only,
            role,
            dry_run,
        } => {
            let mut keys = vec![];
            if let Some(from) = from {
                let text = std::fs::read_to_string(from)
                    .with_context(|| format!("--from {}", from.display()))?;
                keys = directory::parse_file_keys(&text)
                    .with_context(|| format!("{}: not a keys file", from.display()))?;
                if let Some(only) = only {
                    let ids = crate::split_list(only);
                    if let Some(missing) = ids.iter().find(|i| !keys.iter().any(|k| &k.id == *i)) {
                        anyhow::bail!("no key with id {missing} in {}", from.display());
                    }
                    keys.retain(|k| ids.contains(&k.id));
                }
            }
            anyhow::ensure!(
                !(*self_import && to.store.is_some()),
                "--self goes to the server that has your key, signed in with it: memstore auth login --server <url> --api-key <key>"
            );
            let change = Change::ImportKeys {
                org: org_of(org)?,
                team: team.clone(),
                role: directory::KeyRole::parse(role)?,
                keys,
                self_import: *self_import,
            };
            let done = to.change(change, *dry_run).await?;
            if !dry_run && !done.is_empty() {
                println!(
                    "\nThe same keys work as before, now as the directory's: revoke them with `memstore apikey revoke`. Remove them from the keys file when the server next restarts."
                );
            }
        }
        ApikeyCmd::Sources {
            name,
            org,
            sources,
            any,
        } => {
            let change = Change::SetKeySources {
                org: org_of(org)?,
                name: name.clone(),
                sources: match any {
                    true => vec![],
                    false => sources
                        .as_deref()
                        .map(crate::split_list)
                        .unwrap_or_default(),
                },
            };
            to.change(change, false).await?;
        }
        ApikeyCmd::Revoke { name, org } => {
            let change = Change::RevokeKey {
                org: org_of(org)?,
                name: name.clone(),
            };
            to.change(change, false).await?;
        }
    }
    Ok(())
}

/// `memstore audit`: through the server as you (an org admin), or straight from the catalog as its owner.
/// No org: the operator's view, the rows of no org (through the server, with the admin key only).
async fn audit(
    to: &Target<'_>,
    org: Option<&str>,
    since: &str,
    kind: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let since_secs = parse_duration(since).map_err(|_| {
        anyhow::anyhow!("--since {since:?}: a number and a unit, like 24h, 7d, 30m or 45s")
    })?;
    let parsed = kind.map(crate::audit::Kind::parse).transpose()?;
    let (events, warnings): (Vec<Value>, Vec<String>) = match to.store {
        Some(store) => {
            let since = chrono::Utc::now().timestamp_micros() - since_secs * 1_000_000;
            let (events, warnings) = match org {
                Some(org) => {
                    crate::check_name("org", org)?;
                    crate::audit::collect(store, org, since, parsed, vec![]).await?
                }
                None => crate::audit::collect_operator(store, since, parsed, vec![]).await?,
            };
            (events.iter().map(|e| e.to_json()).collect(), warnings)
        }
        None => {
            let server = Config::load()?
                .context()
                .get("server")
                .context("no server: `memstore join <url>`, or `memstore config set server <url>`")?
                .to_string();
            // As the org's own sign-in, if there is one.
            let body = match org {
                Some(org) => {
                    json!({"op": "audit", "org": org, "since_secs": since_secs, "kind": kind})
                }
                None => {
                    json!({"op": "audit", "operator": true, "since_secs": since_secs, "kind": kind})
                }
            };
            let v = client::call_on(&server, org, None, "POST", "/admin", Some(body)).await?;
            (
                v["events"].as_array().cloned().unwrap_or_default(),
                strings(&v["warnings"]),
            )
        }
    };
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if as_json {
        for e in &events {
            println!("{e}");
        }
        return Ok(());
    }
    if events.is_empty() {
        match org {
            Some(org) => println!("no events in {org} in the last {since}"),
            None => println!("no events outside the orgs in the last {since}"),
        }
    }
    for e in &events {
        // Every field escaped: a tab, newline or terminal escape in a name is shown, never acted on.
        let s = |f: &str| crate::audit::escape(e[f].as_str().unwrap_or("-"));
        let n = |f: &str| e[f].as_i64().unwrap_or_default();
        let what = match s("kind").as_str() {
            "refusal" => format!(
                "{} {}: {}{}",
                n("status"),
                s("path"),
                s("detail"),
                if n("count") > 1 {
                    format!(" (x{})", n("count"))
                } else {
                    String::new()
                }
            ),
            "key" => format!(
                "{} on {}{}: {} reads, {} writes in the hour",
                s("path"),
                s("scope"),
                match e["as"].as_str() {
                    Some(_) => format!(" as {}", s("as")),
                    None => String::new(),
                },
                n("reads"),
                n("writes")
            ),
            _ => s("detail"),
        };
        println!("{}  {:<7}  {}  {what}", s("at"), s("kind"), s("actor"));
    }
    Ok(())
}

async fn identity_check() -> Result<()> {
    let checks = crate::server::identity_check().await;
    let mut failed = 0;
    for c in &checks {
        println!(
            "{} {}: {}",
            match (c.ok, c.warn) {
                (false, _) => "FAIL",
                (true, true) => "warn",
                (true, false) => "ok  ",
            },
            c.what,
            c.detail
        );
        failed += usize::from(!c.ok);
    }
    anyhow::ensure!(failed == 0, "{failed} of {} checks failed", checks.len());
    Ok(())
}

/// The keys file's rows; none if it doesn't exist yet.
fn read_keys(path: &PathBuf) -> Result<Vec<Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn write_keys(path: &PathBuf, rows: &[Value]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(rows)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `memstore keys` writes a keys file, which servers read only when they start: deprecated for
/// everything but admin keys, which no directory key can stand for.
const KEYS_DEPRECATED: &str = "deprecated: keys files are read only when the server starts; use `memstore apikey create|revoke` (an org's or team's key, working from its next request), and `memstore apikey import` to move existing file keys. Only admin keys still belong here.";

fn keys(cmd: &KeysCmd) -> Result<()> {
    eprintln!("{KEYS_DEPRECATED}");
    match cmd {
        KeysCmd::New {
            keys_file,
            user,
            groups,
            admin,
            id,
        } => {
            // Stored as compared: an email folds, so the row reads as the server will name the user.
            let user = crate::auth::norm_id(user);
            let id = id.clone().unwrap_or_else(|| user.clone());
            let mut rows = read_keys(keys_file)?;
            anyhow::ensure!(
                !rows.iter().any(|r| r["id"] == id),
                "a key with id {id} exists: revoke it first, or pass --id"
            );
            let key = crate::people::team::new_key();
            rows.push(json!({
                "id": id,
                "sha256": crate::auth::hex(&<sha2::Sha256 as sha2::Digest>::digest(key.as_bytes())),
                "user": user,
                "groups": groups.as_deref().map_or(vec![], crate::split_list),
                "admin": admin,
            }));
            write_keys(keys_file, &rows)?;
            println!("{key}");
            eprintln!(
                "added {id} to {} (restart the server to pick it up)",
                keys_file.display()
            );
        }
        KeysCmd::Revoke { keys_file, id } => {
            let mut rows = read_keys(keys_file)?;
            let before = rows.len();
            rows.retain(|r| &r["id"] != id);
            anyhow::ensure!(
                rows.len() < before,
                "no key with id {id} in {}",
                keys_file.display()
            );
            write_keys(keys_file, &rows)?;
            eprintln!("revoked {id} (restart the server to drop it)");
        }
    }
    Ok(())
}

/// POST JSON to a running server with an admin key; the body, or an error naming the status.
async fn admin_post(url: &str, path: &str, key: &str, body: Value) -> Result<String> {
    let res = reqwest::Client::new()
        .post(format!("{}{path}", url.trim_end_matches('/')))
        .bearer_auth(key)
        .body(body.to_string())
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "{url}{path}: {status} {text}");
    Ok(text)
}

/// `memstore org export`: the files the catalog (or the server) gives, written under `out`.
async fn export(to: &Target<'_>, org: &str, out: &std::path::Path) -> Result<()> {
    let files: Vec<(String, Vec<Value>)> = match to.store {
        Some(store) => crate::offboard::export_org(store, &owner(), None, org).await?,
        None => {
            let body = json!({"op": "export_org", "org": org});
            let v = client::call("POST", "/admin", Some(body)).await?;
            v["files"]
                .as_object()
                .context("the server sent no files")?
                .iter()
                .map(|(p, rows)| (p.clone(), rows.as_array().cloned().unwrap_or_default()))
                .collect()
        }
    };
    for (path, rows) in &files {
        // Paths come from the server: only plain relative ones, inside `out`.
        let rel = std::path::Path::new(path);
        let plain = rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        anyhow::ensure!(
            plain,
            "refusing to write {path:?}: not a plain relative path"
        );
        let dest = out.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for r in rows {
            text.push_str(&serde_json::to_string(r)?);
            text.push('\n');
        }
        std::fs::write(&dest, text).with_context(|| dest.display().to_string())?;
        println!("{}\t{} rows", dest.display(), rows.len());
    }
    Ok(())
}

async fn users(cmd: &UsersCmd) -> Result<()> {
    let (url, user, key, disabled) = match cmd {
        UsersCmd::Disable { url, user, key } => (url, user, key, true),
        UsersCmd::Enable { url, user, key } => (url, user, key, false),
        UsersCmd::Forget { .. } => unreachable!("forget goes to the directory"),
    };
    admin_post(
        url,
        "/closure",
        key,
        json!({"user": user, "disabled": disabled}),
    )
    .await?;
    println!("{user} {}", if disabled { "disabled" } else { "enabled" });
    Ok(())
}

async fn acl(cmd: &AclCmd) -> Result<()> {
    let AclCmd::Set {
        url,
        acl,
        source_prefix,
        source,
        episode,
        key,
    } = cmd;
    let mut body = json!({"acl": crate::split_list(acl)});
    match (source_prefix, source, episode) {
        (Some(p), _, _) => body["source_prefix"] = p.as_str().into(),
        (_, Some(s), _) => body["source"] = s.as_str().into(),
        (_, _, Some(e)) => body["episode_id"] = e.as_str().into(),
        _ => anyhow::bail!("say which episodes: --source-prefix, --source or --episode"),
    }
    println!("{}", admin_post(url, "/acl", key, body).await?);
    Ok(())
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// The org a command means: --org, else the context's org, else the default org.
fn org_of(org: &Option<String>) -> Result<String> {
    Ok(match org {
        Some(o) => o.clone(),
        None => Config::load()?
            .get("org")
            .unwrap_or_else(|| crate::DEFAULT_ORG.to_string()),
    })
}

/// Where directory changes go: the catalog (as its owner), or the signed-in server (as you).
struct Target<'a> {
    store: Option<&'a Store>,
}

impl Target<'_> {
    /// Make (or with `dry_run`, show) a change and print what changed.
    async fn change(&self, mut change: Change, dry_run: bool) -> Result<Vec<String>> {
        let changes: Vec<String> = match self.store {
            Some(store) => {
                // Straight to the catalog: this machine seals a client secret, with the server's key.
                let key = crate::auth::secret::SecretKey::from_env()?;
                directory::seal_secrets(&mut change, key.as_ref())?;
                let mut actor = owner();
                // One key, one caller: a new key is none of the keys file's, here as on a server.
                if matches!(change, Change::CreateKey { .. }) {
                    actor.file_keys = direct_file_keys()?;
                }
                let events = if dry_run {
                    directory::plan(&Directory::load(store).await?, &actor, &change)?
                } else {
                    directory::apply(store, &actor, &change).await?
                };
                events.iter().map(|e| e.describe()).collect()
            }
            None => {
                let mut body = serde_json::to_value(&change)?;
                body["dry_run"] = json!(dry_run);
                strings(&client::call("POST", "/admin", Some(body)).await?["changes"])
            }
        };
        if changes.is_empty() {
            println!("nothing to change");
        }
        for c in &changes {
            println!("{}{c}", if dry_run { "would: " } else { "" });
        }
        Ok(changes)
    }

    /// The list of orgs (None) or one org.
    async fn view(&self, org: Option<&str>) -> Result<Value> {
        match self.store {
            Some(store) => {
                let dir = Directory::load(store)
                    .await?
                    .with_usage(&store.key_usage().await?);
                Ok(match org {
                    None => json!(dir.orgs.keys().collect::<Vec<_>>()),
                    Some(o) => json!(dir.orgs.get(o).with_context(|| format!("no org {o}"))?),
                })
            }
            None => {
                let v =
                    client::call("POST", "/admin", Some(json!({"op": "show", "org": org}))).await?;
                Ok(v["view"].clone())
            }
        }
    }

    /// An org's schema on `branch` (null when untyped), and with `history` every version there.
    async fn schema_of(&self, org: &str, branch: &str, history: bool) -> Result<Value> {
        match self.store {
            Some(store) => {
                let scope = crate::Scope::org(org)?;
                let mut out = json!({"schema": store.schema(&scope, branch).await?.as_deref()});
                if history {
                    out["history"] = json!(
                        store
                            .schema_history(&scope, branch)
                            .await?
                            .iter()
                            .map(|s| &**s)
                            .collect::<Vec<_>>()
                    );
                }
                Ok(out)
            }
            None => client::call(
                "POST",
                "/admin",
                Some(json!({"op": "get_schema", "org": org, "branch": branch, "history": history})),
            )
            .await,
        }
    }

    /// Type an org's memory with `schema` (a built-in name or a JSON file), through the server; with `dry_run`,
    /// make the same checks and commit nothing.
    async fn put_schema(&self, org: &str, schema: &str, dry_run: bool) -> Result<()> {
        anyhow::ensure!(
            self.store.is_none(),
            "org set-schema goes through a server: for a catalog, `memstore schema set org:{org} {schema}`"
        );
        let s = crate::query::schema::Schema::load(schema)?;
        let v = client::call(
            "POST",
            "/admin",
            Some(json!({"op": "set_schema", "org": org, "schema": s, "dry_run": dry_run})),
        )
        .await?;
        for c in strings(&v["changes"]) {
            println!("{}{c}", if dry_run { "would: " } else { "" });
        }
        Ok(())
    }

    /// Type an org's memory with a built-in schema.
    async fn set_schema(&self, org: &str, kind: &str) -> Result<()> {
        match self.store {
            Some(store) => {
                let schema = crate::query::schema::Schema::load(kind)?;
                store
                    .set_schema(
                        &crate::Scope::org(org)?,
                        crate::MAIN,
                        &owner().user,
                        &schema,
                        false,
                    )
                    .await?;
                println!(
                    "{org} memory uses schema {} v{}",
                    schema.name, schema.version
                );
            }
            None => {
                let v = client::call(
                    "POST",
                    "/admin",
                    Some(json!({"op": "set_schema", "org": org, "schema": kind})),
                )
                .await?;
                for c in strings(&v["changes"]) {
                    println!("{c}");
                }
            }
        }
        Ok(())
    }
}

/// sha256 of the keys in this machine's keys file (`MEMSTORE_API_KEYS`), if it names one: what a key made with
/// --direct may not be. A keys file named but unreadable refuses the change, rather than skipping the check.
fn direct_file_keys() -> Result<std::collections::BTreeSet<String>> {
    let Some(path) = std::env::var("MEMSTORE_API_KEYS")
        .ok()
        .filter(|p| !p.is_empty())
    else {
        return Ok(Default::default());
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("MEMSTORE_API_KEYS {path}"))?;
    Ok(directory::parse_file_keys(&text)
        .with_context(|| format!("MEMSTORE_API_KEYS {path}"))?
        .into_iter()
        .map(|k| k.sha256.to_ascii_lowercase())
        .collect())
}

/// Reaching the catalog directly is owning it.
pub fn owner() -> Actor {
    Actor {
        user: format!(
            "cli:{}",
            std::env::var("USER").unwrap_or_else(|_| "owner".into())
        ),
        superuser: true,
        owner: true,
        ..Default::default()
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(String::from))
        .collect()
}

async fn invite(to: &Target<'_>, a: &InviteArgs) -> Result<()> {
    let org = org_of(&a.org)?;
    let role = Role::parse(&a.role)?;
    let mut members: Vec<SyncMember> = a
        .email
        .iter()
        .map(|e| SyncMember {
            user: e.clone(),
            role,
            teams: vec![],
        })
        .collect();
    if let Some(f) = &a.file {
        members.extend(directory::parse_members(&std::fs::read_to_string(f)?)?);
    }
    anyhow::ensure!(!members.is_empty(), "invite whom? --email or --file");
    if let Some(t) = &a.team {
        for m in &mut members {
            if !m.teams.contains(t) {
                m.teams.push(t.clone());
            }
        }
    }
    let n = members.len();
    to.change(
        Change::Invite {
            org: org.clone(),
            members,
        },
        false,
    )
    .await?;
    println!("\nInvited {n}. Share this with them:");
    let server = Config::load()?
        .get("server")
        .unwrap_or_else(|| "<server url>".into());
    let team = a.team.as_deref();
    println!(
        "  memstore join {server} --org {org}{}",
        team.map(|t| format!(" --team {t}")).unwrap_or_default()
    );
    println!(
        "  claude mcp add --transport http memstore '{server}/mcp?org={org}{}'",
        team.map(|t| format!("&team={t}")).unwrap_or_default()
    );
    Ok(())
}

async fn join(
    server: &str,
    org: Option<&str>,
    team: Option<&str>,
    no_browser: bool,
    sso: bool,
) -> Result<()> {
    client::join(server, org, team, no_browser, sso).await
}

/// `memstore setup`: sign in unless already signed in to `org` on `server`, install memstore in each coding agent
/// (`harnesses`, else every one found), then `memstore doctor`.
async fn set_up_machine(
    server: &str,
    org: Option<&str>,
    team: Option<&str>,
    harnesses: &[String],
    no_browser: bool,
    dry: bool,
) -> Result<()> {
    use crate::hook::Harness;
    use crate::hook_install::{self, Mcp};
    let server = server.trim_end_matches('/');
    let agents: Vec<Harness> = match harnesses {
        [] => hook_install::detected(),
        named => named
            .iter()
            .map(|h| Harness::parse(h))
            .collect::<Result<_>>()?,
    };
    if agents.is_empty() {
        anyhow::bail!(
            "no coding agent found here (Claude Code, Codex, Gemini CLI, Cursor or Copilot): name one with --harness"
        );
    }
    // Already signed in there, as a member of the org: keep that sign-in and point the context at it.
    let signed_in = match org {
        Some(o) => client::call_on(server, Some(o), team, "GET", "/me", None)
            .await
            .is_ok_and(|me| me["memberships"].get(o).is_some()),
        None => false,
    };
    match (dry, signed_in) {
        (true, true) => println!("signed in to {server} already: would keep it"),
        (true, false) => println!("would sign in to {server} (memstore join)"),
        (false, true) => {
            let mut ctx = Context::default();
            ctx.set("server", Some(server))?;
            ctx.set("org", org)?;
            ctx.set("team", team)?;
            let name = ctx.default_name();
            let mut config = Config::load()?;
            config.put_context(&name, ctx);
            config.save()?;
            println!("Signed in to {server} already; switched to context {name}");
        }
        (false, false) => join(server, org, team, no_browser, false).await?,
    }
    let lead = if dry { "would change" } else { "changed" };
    for h in &agents {
        let changes = match h {
            Harness::ClaudeCode => hook_install::claude_code(false, dry)?,
            h => hook_install::apply_with(*h, None, Mcp::SignedIn, false, dry)?,
        };
        if changes.is_empty() {
            println!("{}: already set up", h.name());
        }
        for c in &changes {
            println!("{lead} {}: {}", c.path.display(), c.what);
        }
    }
    if dry {
        return Ok(());
    }
    println!("restart each agent to load memstore. Checking:");
    crate::hook::doctor().await
}

/// Suggested answers, by the kind of memory.
fn suggestions(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "sales" => (
            "deals at risk and why; customer commitments and next steps; objections and competitors; champions and decision makers",
            "slack, email, calls, crm",
        ),
        "engineering" => (
            "decisions and the reasons for them; who owns what; incidents and their fixes; roadmap and scope changes",
            "slack, github, docs",
        ),
        _ => (
            "decisions and the reasons for them; who owns what; commitments and deadlines",
            "slack, docs",
        ),
    }
}

/// Ask on the terminal (stderr), reading one line of stdin; `given` (a flag) skips the question, and
/// `yes` or an empty answer takes `default`.
fn ask(
    n: usize,
    question: &str,
    given: &Option<String>,
    default: &str,
    yes: bool,
) -> Result<String> {
    if let Some(g) = given {
        return Ok(g.clone());
    }
    if yes {
        return Ok(default.to_string());
    }
    let hint = if default.is_empty() {
        String::new()
    } else {
        format!(" [{default}]")
    };
    eprint!("{n}/6 {question}{hint}\n> ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default } else { line }.to_string())
}

fn list(s: &str, sep: char) -> Vec<String> {
    // Focus items may hold commas; `;` separates them when present.
    let sep = if sep == ';' && !s.contains(';') {
        ','
    } else {
        sep
    };
    s.split(sep)
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// `memstore org init`: what the org wants from memory, stored as its profile (agents read it when they
/// connect), the schema that fits, and the teams to start with.
async fn init(to: &Target<'_>, a: &InitArgs) -> Result<()> {
    let org = org_of(&a.org)?;
    let yes = a.yes || !std::io::stdin().is_terminal() && a.description.is_some();
    eprintln!("Setting up memory for {org}. Enter takes the suggestion in brackets.\n");
    let description = ask(
        1,
        &format!("What does {org} do, in a sentence?"),
        &a.description,
        "",
        yes,
    )?;
    let kind = ask(
        2,
        "Which fits your memory best: sales (accounts, deals, meetings), engineering (features, decisions, incidents) or general?",
        &a.kind,
        "general",
        yes,
    )?
    .to_lowercase();
    anyhow::ensure!(
        ["sales", "engineering", "general"].contains(&kind.as_str()),
        "kind is sales, engineering or general"
    );
    let (focus_default, sources_default) = suggestions(&kind);
    let focus = ask(
        3,
        "What should memory pay most attention to?",
        &a.focus,
        focus_default,
        yes,
    )?;
    let avoid = ask(
        4,
        "What must memory never keep?",
        &a.avoid,
        "passwords and secrets; health and compensation details",
        yes,
    )?;
    let sources = ask(
        5,
        "Where does your knowledge live?",
        &a.sources,
        sources_default,
        yes,
    )?;
    let teams = ask(
        6,
        "Teams to create now (comma-separated)?",
        &a.teams,
        "",
        yes,
    )?;
    let profile = Profile {
        description,
        kind: kind.clone(),
        focus: list(&focus, ';'),
        avoid: list(&avoid, ';'),
        sources: list(&sources, ','),
    };
    eprintln!();
    to.change(
        Change::SetProfile {
            org: org.clone(),
            profile: profile.clone(),
        },
        false,
    )
    .await?;
    if kind != "general" {
        to.set_schema(&org, &kind).await?;
    }
    for t in list(&teams, ',') {
        let r = to
            .change(
                Change::CreateTeam {
                    org: org.clone(),
                    team: t.clone(),
                },
                false,
            )
            .await;
        if let Err(e) = r
            && !e.to_string().contains("already exists")
        {
            return Err(e);
        }
    }
    let server = Config::load()?
        .get("server")
        .unwrap_or_else(|| "<server url>".into());
    println!(
        "\nAgents connecting to {org} are told:\n  {}",
        profile.instructions(&org)
    );
    println!("\nNext:");
    println!("  memstore invite --email <email> [--team <team>]   # or --file people.csv");
    for s in &profile.sources {
        let how = match s.as_str() {
            "slack" => "export channels, or have the AmpUp writer stream them",
            "github" => "issues and PRs through a GitHub export",
            "email" | "calls" | "crm" => {
                "load through your warehouse (memstore load / the SQL remember procedure)"
            }
            _ => "memstore ingest, or memstore load for a directory of episodes",
        };
        println!("  {s}: {how}");
    }
    println!("  claude mcp add --transport http memstore '{server}/mcp?org={org}'");
    Ok(())
}

async fn auth(to: &Target<'_>, a: &AuthCmd) -> Result<()> {
    match a {
        AuthCmd::Login {
            server,
            api_key,
            issuer,
            client_id,
            client_secret,
            no_browser,
            port,
        } => {
            let mut config = Config::load()?;
            if let Some(s) = server {
                config.set("server", Some(s))?;
                config.save()?;
            }
            let server = config.get("server");
            if let Some(key) = api_key {
                let server = server.context("an API key signs in to a server: --server")?;
                let c = Credentials::api_key(&server, key).await?;
                println!("Signed in to {server} as {} (API key)", c.user);
                return Ok(());
            }
            let opts = client::LoginOptions {
                issuer: issuer.clone(),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                no_browser: *no_browser,
                org: config.get("org"),
                // `auth login` is the org's own sign-in: its Organization where it has one.
                sso: true,
                port: *port,
            };
            let creds = client::login(server.as_deref(), &opts).await?;
            println!("Signed in as {} ({})", creds.user, creds.issuer);
            if let Some(o) = creds.org() {
                println!("Signed in for {o}: this sign-in counts in {o} only");
            }
            if config.get("org").is_none() {
                println!("Choose your memory: memstore use <org> (or <org>/<team>)");
            }
        }
        AuthCmd::Logout => {
            let server = Config::load()?
                .get("server")
                .context("no server in this context")?;
            let out = if Credentials::remove(&server)? {
                "Signed out"
            } else {
                "Not signed in"
            };
            println!("{out}");
        }
        AuthCmd::Status => match Credentials::load(None)? {
            None => println!("Not signed in"),
            Some(c) => {
                println!(
                    "Signed in as {} ({})",
                    c.user,
                    if c.api_key.is_some() {
                        "API key"
                    } else {
                        &c.issuer
                    }
                );
                print_json(&client::call("GET", "/me", None).await?)?;
            }
        },
        AuthCmd::PrintToken => {
            let mut c =
                Credentials::load(None)?.context("not signed in: run `memstore auth login`")?;
            println!("{}", c.bearer().await?);
        }
        AuthCmd::Setup(s) => setup(to, s).await?,
        AuthCmd::RemoveProvider { org } => {
            to.change(Change::RemoveProvider { org: org_of(org)? }, false)
                .await?;
        }
        AuthCmd::Domain(DomainCmd::Add { domain, team, org }) => {
            let change = Change::AddDomain {
                org: org_of(org)?,
                domain: domain.clone(),
                team: team.clone(),
            };
            to.change(change, false).await?;
        }
        AuthCmd::Domain(DomainCmd::Remove { domain, org }) => {
            let change = Change::RemoveDomain {
                org: org_of(org)?,
                domain: domain.clone(),
            };
            to.change(change, false).await?;
        }
        AuthCmd::MapGroup { group, team, org } => {
            let change = Change::MapGroup {
                org: org_of(org)?,
                group: group.clone(),
                team: team.clone(),
            };
            to.change(change, false).await?;
        }
        AuthCmd::UnmapGroup { group, team, org } => {
            let change = Change::UnmapGroup {
                org: org_of(org)?,
                group: group.clone(),
                team: team.clone(),
            };
            to.change(change, false).await?;
        }
    }
    Ok(())
}

/// `memstore auth setup`: connect the org's provider, then (with --test) sign in through it and show what
/// it says about you and what that gets you.
async fn setup(to: &Target<'_>, a: &SetupArgs) -> Result<()> {
    let org = org_of(&a.org)?;
    if let Some(kind) = &a.provider {
        let issuer = match (kind.as_str(), &a.issuer, &a.tenant) {
            (_, Some(i), _) => i.clone(),
            ("google", None, _) => "https://accounts.google.com".into(),
            ("entra", None, Some(t)) => format!("https://login.microsoftonline.com/{t}/v2.0"),
            ("entra", None, None) => anyhow::bail!("entra needs --tenant (or --issuer)"),
            (_, None, _) => anyhow::bail!("{kind} needs --issuer"),
        };
        anyhow::ensure!(
            !a.client_ids.is_empty(),
            "--client-id: the client registered with {kind} for memstore"
        );
        // With no secret and nothing said, the CLI's client signs in with PKCE alone: a public client.
        let client_type = match (a.public_client, a.confidential_client, &a.client_secret) {
            (_, true, _) => directory::ClientType::Confidential,
            (true, _, _) | (false, false, None) => directory::ClientType::Public,
            (false, false, Some(_)) => None.context(
                "--client-secret: say what kind of client this is, --public-client (an installed app, \
                 whose secret is handed to anyone who asks how to sign in) or --confidential-client \
                 (whose secret is never taken)",
            )?,
        };
        let provider = directory::Provider {
            kind: kind.clone(),
            issuer,
            client_ids: a.client_ids.clone(),
            client_type: Some(client_type),
            client_secret: a.client_secret.clone(),
            groups_claim: a.groups_claim.clone(),
            required: a.require,
        };
        // Refused here too, so a confidential client's secret never leaves this machine.
        if let Some(why) = directory::client_type_problem(&provider) {
            anyhow::bail!("{why}");
        }
        to.change(
            Change::SetProvider {
                org: org.clone(),
                provider,
            },
            false,
        )
        .await?;
        if !a.test {
            println!("Members sign in with: memstore auth login (with org {org} selected)");
            println!("Try it first: memstore auth setup --org {org} --test");
        }
    }
    if a.test {
        let server = Config::load()?
            .get("server")
            .context("--test signs in to the server: memstore config set server <url>")?;
        let opts = client::LoginOptions {
            no_browser: a.no_browser,
            org: Some(org.clone()),
            ..Default::default()
        };
        let creds = client::login(Some(&server), &opts).await?;
        println!("Signed in as {} ({})", creds.user, creds.issuer);
        if creds.org().is_none() {
            println!("{org} has no identity provider of its own: this was the server's");
        }
        let c = creds.claims();
        let groups_claim = a.groups_claim.as_str();
        println!("The provider says:");
        for k in ["email", "email_verified", groups_claim, "iss", "aud"] {
            println!(
                "  {k}: {}",
                c.get(k).map_or("(not sent)".into(), |v| v.to_string())
            );
        }
        let me = client::call_on(&server, Some(&org), None, "GET", "/me", None).await?;
        match me["memberships"].get(&org) {
            Some(m) => println!("In {org}: {m}"),
            None => println!(
                "In {org}: nothing{}",
                me["refused"]
                    .as_str()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ),
        }
    }
    anyhow::ensure!(
        a.provider.is_some() || a.test,
        "connect a provider (--provider ...) or try the one there is (--test)"
    );
    Ok(())
}

fn config(c: &ConfigCmd) -> Result<()> {
    let mut config = Config::load()?;
    match c {
        ConfigCmd::GetContexts => {
            let current = config.current_name();
            println!("CURRENT\tNAME\tSERVER\tORG\tTEAM");
            for (name, ctx) in &config.contexts {
                println!(
                    "{}\t{name}\t{}\t{}\t{}",
                    if current.as_deref() == Some(name) {
                        "*"
                    } else {
                        ""
                    },
                    ctx.server.as_deref().unwrap_or("-"),
                    ctx.org.as_deref().unwrap_or("-"),
                    ctx.team.as_deref().unwrap_or("-"),
                );
            }
            return Ok(());
        }
        ConfigCmd::CurrentContext => {
            println!("{}", config.current_name().context("no current context")?);
            return Ok(());
        }
        ConfigCmd::View => return print_json(&serde_json::to_value(&config)?),
        ConfigCmd::Get { key } => {
            println!("{}", config.get(key).unwrap_or_default());
            return Ok(());
        }
        ConfigCmd::UseContext { name } => config.use_context(name)?,
        ConfigCmd::SetContext {
            name,
            server,
            org,
            team,
            user,
            use_it,
        } => {
            let mut ctx = config.contexts.get(name).cloned().unwrap_or_default();
            for (k, v) in [
                ("server", server),
                ("org", org),
                ("team", team),
                ("user", user),
            ] {
                if let Some(v) = v {
                    ctx.set(k, Some(v))?;
                }
            }
            config.contexts.insert(name.clone(), ctx);
            if *use_it || config.current.is_none() {
                config.current = Some(name.clone());
            }
        }
        ConfigCmd::DeleteContext { name } => {
            anyhow::ensure!(config.contexts.remove(name).is_some(), "no context {name}");
            if config.current.as_deref() == Some(name) {
                config.current = None;
            }
        }
        ConfigCmd::Set { key, value } => config.set(key, Some(value))?,
        ConfigCmd::Unset { key } => config.set(key, None)?,
    }
    config.save()
}

async fn remote(r: &RemoteCmd) -> Result<()> {
    let v = match r {
        RemoteCmd::Me => client::call("GET", "/me", None).await?,
        RemoteCmd::Search { text, k } => {
            let body = json!({"data": [[0, text.join(" "), k]]});
            client::call("POST", "/search", Some(body)).await?["data"][0][1].clone()
        }
        RemoteCmd::Query { query } => {
            let text = match query.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)?,
                None => query.clone(),
            };
            let ir: Value = serde_json::from_str(&text)?;
            let body = json!({"data": [[0, ir]]});
            client::call("POST", "/query", Some(body)).await?["data"][0][1].clone()
        }
    };
    print_json(&v)
}

/// `memstore team check|keys|load`: a team pilot from a csv.
async fn pilot(cmd: &TeamCmd) -> Result<()> {
    use crate::people::team::{Team, issue_keys};
    let read = |p: &PathBuf| {
        std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
    };
    match cmd {
        TeamCmd::Create { .. }
        | TeamCmd::Delete { .. }
        | TeamCmd::List { .. }
        | TeamCmd::Add { .. }
        | TeamCmd::Remove { .. } => unreachable!("directory commands"),
        TeamCmd::Check { csv } => {
            let team = Team::parse(&read(csv)?)?;
            for (email, closure) in team.closures() {
                let p = team.get(&email).unwrap();
                println!(
                    "{email} ({}; manager {}): {}",
                    p.name,
                    p.manager.as_deref().unwrap_or("-"),
                    closure.join(" ")
                );
            }
            println!("{} people, ok", team.people().len());
        }
        TeamCmd::Keys {
            csv,
            keys_file,
            only,
            rotate,
        } => {
            let team = Team::parse(&read(csv)?)?;
            let existing: Vec<serde_json::Value> = match std::fs::read_to_string(keys_file) {
                Ok(t) => serde_json::from_str(&t)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", keys_file.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(e) => anyhow::bail!("{}: {e}", keys_file.display()),
            };
            let only = only.as_deref().map_or(vec![], crate::split_list);
            let (issued, rows) = issue_keys(&team, &existing, &only, *rotate)?;
            let tmp = keys_file.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&rows)?)?;
            std::fs::rename(&tmp, keys_file)?;
            for (email, key) in &issued {
                println!("{email} {key}");
            }
            eprintln!(
                "{} keys issued, {} rows in {} (restart the server to pick them up)",
                issued.len(),
                rows.len(),
                keys_file.display()
            );
        }
        TeamCmd::Load { csv, url, key } => {
            let text = read(csv)?;
            Team::parse(&text)?; // say what's wrong here, not through the server
            let res = reqwest::Client::new()
                .post(format!("{}/team", url.trim_end_matches('/')))
                .bearer_auth(key)
                .body(text)
                .send()
                .await?;
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            anyhow::ensure!(status.is_success(), "{url}/team: {status} {body}");
            println!("{body}");
        }
    }
    Ok(())
}
