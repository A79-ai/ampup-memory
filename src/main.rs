use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use memstore::harness::corpus;
use memstore::{Episode, Fact, Principal, Scope, Store, platform, smoke};

/// Adaptive Memory Store: facts in the customer's Iceberg catalog, read and written through a
/// ledger.
///
/// The catalog comes from MEMSTORE_PLATFORM (local | rest | gcp | snowflake | databricks; see the
/// README). A scope is `org`, `org:<org>`, `team:<id>`, `team:<org>/<team>` or `user:<id>`. A reader
/// is `user` or `user:group,...`.
///
/// The release this was built as (`v0.1.0-rc.7`, from the release workflow), else the crate's version.
const VERSION: &str = match option_env!("MEMSTORE_RELEASE") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// People and orgs: `create-org`, `org init`, `create-team`, `invite`, `requests`, `decline`, `join`, `use`. Signed in to a
/// server they go to it as you; otherwise (or with --direct) they reach the catalog as its owner.
/// Contexts (server, org, team) live in ~/.memstore, like kubectl's.
#[derive(Parser)]
#[command(name = "memstore", version = VERSION, propagate_version = true)]
struct Cli {
    /// Directory commands go to the catalog (as its owner) even when signed in to a server
    #[arg(long, global = true)]
    direct: bool,
    /// Use this context for one command (also MEMSTORE_CONTEXT)
    #[arg(long, global = true)]
    context: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write and read back a small fixture, checking visibility, as-of reads and branches
    Smoke { scope: String },
    /// Record one fact as its own episode
    Put {
        scope: String,
        /// Who may read it: `*`, `user:<id>`, `group:<id>`, comma-separated
        acl: String,
        entity: String,
        predicate: String,
        value: String,
    },
    /// Record one episode (no facts)
    Ingest {
        scope: String,
        source: String,
        acl: String,
        body: String,
    },
    /// End a fact at a time; the reason is the closure's evidence, its own episode readable by `acl`
    /// (readers who can't see it still see the fact as it was)
    Close {
        scope: String,
        fact_id: String,
        /// When the fact stopped being true (RFC 3339 or YYYY-MM-DD)
        when: String,
        acl: String,
        #[arg(required = true, num_args = 1..)]
        reason: Vec<String>,
    },
    /// Every fact a reader can see, tab-separated: entity, predicate, value, fact id
    Get {
        scope: String,
        user: String,
        /// The reader's groups (each may be a comma-separated list)
        groups: Vec<String>,
    },
    /// Read latency, then how soon a write is visible to another warm reader
    Bench {
        scope: String,
        #[arg(default_value_t = 10)]
        reads: usize,
        #[arg(default_value_t = 0)]
        gap_ms: u64,
    },
    /// Generate a synthetic corpus with gold questions (`-` only counts and hashes)
    Gen {
        seed: u64,
        episodes: usize,
        dir: String,
    },
    /// Every fact and gold answer in a corpus cites an existing episode
    VerifyCorpus { dir: PathBuf },
    /// Load a corpus directory (episodes.jsonl, facts.jsonl)
    Load {
        dir: PathBuf,
        #[arg(default_value_t = 5000)]
        episodes_per_commit: usize,
        /// Load only this scope, or `questions` for the scopes the gold questions use
        only: Option<String>,
        /// Into this workspace: the corpus's `org` scope becomes the workspace's, `team:<t>` its team `t`
        #[arg(long)]
        workspace: Option<String>,
        /// Commit on this branch (it must exist) instead of main
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
        /// Refuse unless every scope it writes to (every scope of --workspace) is empty
        #[arg(long)]
        fresh: bool,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Answer a corpus's gold questions through the fact-key API
    Eval {
        dir: PathBuf,
        /// Only this scope, `questions`, or `all`
        scope: Option<String>,
        /// Answer everything once untimed first, so latency is measured warm
        #[arg(long)]
        warm: bool,
    },
    /// Bring a scope's fact and passage indexes up to date (`index <scope>`), or wait for them, check,
    /// rebuild, verify, list and drop them
    Index(IndexArgs),
    /// Schemas: list the built-in ones, read or set a scope's
    #[command(subcommand)]
    Schema(SchemaCmd),
    /// The episode types (sales_call, slack_thread, email, crm_update, doc): their catalogs and how they cite
    #[command(subcommand)]
    EpisodeType(EpisodeTypeCmd),
    /// What a deployment may send its control plane (the telemetry contract)
    #[command(subcommand)]
    Telemetry(TelemetryCmd),
    /// This install's identity with its control plane: enroll, rotate its key, check it
    #[command(subcommand)]
    Deployment(DeploymentCmd),
    /// The deployment agent: a process apart from the engine that applies the control plane's signed
    /// desired state (image, flags) to the engine's workload and runs its allowlisted actions
    Agent(Box<AgentArgs>),
    /// Set up this install with the onboarding wizard: print a pairing code; once paired and planned, create
    /// (or adopt) what the plan lists in your cloud and enroll
    #[cfg(feature = "control-plane")]
    Init(memstore::onboard::InitArgs),
    /// Run a typed query (JSON, or @file)
    Query {
        scope: String,
        user: String,
        query: String,
    },
    /// Text search over facts
    Search {
        scope: String,
        user: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Many searches in one warm process (benchmarks): JSON lines in, JSON lines out
    SearchBatch { input: PathBuf, output: PathBuf },
    /// Read service: POST /query, /search, /episode and /mcp
    Serve {
        #[arg(default_value = "org")]
        scope: String,
        #[arg(default_value_t = 8080)]
        port: u16,
        /// Groups every caller reads as when the service does not identify callers
        groups: Option<String>,
        /// Run on this machine with no cloud and no Docker: tables, index and catalog under DIR
        /// (default ~/.memstore), one caller, MCP at /mcp, bound to 127.0.0.1
        #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "~/.memstore")]
        local: Option<String>,
        /// Also serve every workspace side by side, each selected with x-memstore-org or ?org= (also
        /// MEMSTORE_SERVE_WORKSPACES=1)
        #[arg(long)]
        workspaces: bool,
    },
    /// Expire all but the last `keep` Iceberg snapshots of the system tables and a scope's tables, and delete the
    /// metadata only they named (every commit does this as it goes, and serve on a timer)
    Maintain {
        scope: String,
        #[arg(default_value_t = 10)]
        keep: usize,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Rewrite the system ledger into one data file, keeping every row, in one commit, then expire every older
    /// snapshot and delete the files only they referenced, so a cold read of it reads a few files instead of one
    /// per commit. Servers keep running and never wait: ledger commits landing meanwhile are carried over. Prints
    /// the rows and a checksum as read, as carried over and as committed, and commits nothing unless the new files
    /// hold exactly the rows read. Run again after an interrupted run: it finishes that one's deletes first.
    /// Logs each step (read, write, catch-up, swap, sweep) with its time to stderr. `serve` does this in its
    /// nightly maintenance window up to MEMSTORE_LEDGER_COMPACT_MAX_FILES data files; past that, run this once
    CompactLedger {
        /// Report the ledger's files now and what a compaction would leave, writing nothing
        #[arg(long)]
        dry_run: bool,
        /// Compact this scope's summaries (its `aggregates` table: a commit per summariser pass) instead
        #[arg(long, value_name = "SCOPE")]
        aggregates: Option<String>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Register memstore's hooks and MCP server in a coding agent: `memstore hooks install --harness codex`
    /// (codex, gemini, cursor or copilot; Claude Code installs its plugin). `uninstall` takes them out
    Hooks {
        /// install or uninstall
        action: String,
        #[arg(long)]
        harness: String,
        /// The memstore server, written into the hooks and MCP entry (MEMSTORE_URL otherwise, read when they run)
        #[arg(long)]
        url: Option<String>,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// A coding agent's hook (`memstore hooks install` registers them; the Claude Code plugin too):
    /// session-start, prompt, stop, session-end or status, reading the hook's JSON on stdin and talking to
    /// MEMSTORE_URL; `outcome good|bad [why]` rates the session's run (`/memstore:outcome`). Never fails the
    /// session: errors go to stderr and it exits 0
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
    /// The server's MCP endpoint over stdio, signed in as the CLI is (`memstore join`): for MCP clients
    /// that start a command, such as the Claude Code plugin. MEMSTORE_URL and MEMSTORE_TOKEN override
    Mcp,
    /// Whether this machine's memory works: the server and whether it's up, who is signed in and until when, the
    /// org, what the memory runs there (extraction, compiling, advice and its evidence bar), and how the last hook
    /// went. Exits non-zero when the server can't be used
    Doctor,
    /// Whether a cloud project is ready for memstore, before anything is created in it: read-only checks of
    /// tools, sign-in, billing, APIs, org policies, quotas, names, egress and permissions, each PASS, WARN or FAIL
    /// with its fix. Exit 0 all pass, 1 warnings, 2 failures, 3 couldn't run
    #[command(subcommand)]
    Preflight(PreflightCmd),
    /// A support bundle for a ticket, made here and written to a directory, never sent: the engine's health,
    /// telemetry, metrics, index status and config as the agent's diagnostics read them (counts and closed
    /// sets, no memory), and a redacted summary of its logs (counts by severity, the error and warning lines
    /// with addresses, keys and emails blanked)
    SupportBundle {
        /// The engine (MEMSTORE_URL otherwise)
        #[arg(long, env = "MEMSTORE_URL")]
        engine: String,
        /// The operator's admin key, in a file (the agent's MEMSTORE_AGENT_ADMIN_KEY_FILE)
        #[arg(long, value_name = "FILE")]
        admin_key_file: Option<PathBuf>,
        /// Where to write it (default ./memstore-bundle-<time>)
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Read the engine's Cloud Run logs with gcloud: the project, with --region and --service
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "us-central1", requires = "project")]
        region: String,
        /// The Cloud Run service (default <name>-engine)
        #[arg(long, default_value = "memstore-engine", requires = "project")]
        service: String,
        /// Or the engine's log lines from a file (`kubectl logs … > engine.log`)
        #[arg(long, value_name = "FILE", conflicts_with = "project")]
        logs_file: Option<PathBuf>,
        /// How far back to read logs (minutes)
        #[arg(long, default_value_t = 60)]
        since_minutes: u32,
    },
    /// Read the episodes the scope schema's `extract` section names that it hasn't read yet, and commit the
    /// facts found; with MEMSTORE_EXTRACT_GOAL_BLIND=1, the rest too. MEMSTORE_EXTRACT picks the backend
    /// (default OpenRouter, Claude Haiku 5.5)
    Extract {
        #[arg(default_value = "org")]
        scope: String,
        /// Most episodes to read in this run
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Print what the run would add and end, one JSON line per episode, and commit nothing
        #[arg(long)]
        dry_run: bool,
        /// Extract with this branch's schema (a workspace branch an arm's schema is set on), committing the
        /// facts there; goal-blind extraction runs on main only
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Dream over the scope's open facts: end values a newer fact replaced, link facts that say the same thing and
    /// link names of one thing as aliases, per shared audience, weighing only what came in since the last pass.
    /// Writes only new rows (records, closures, links, aliases), all withdrawable as one actor. MEMSTORE_DREAM
    /// picks the backend (else MEMSTORE_EXTRACT's; default OpenRouter, Claude Haiku 5.5)
    Dream {
        #[arg(default_value = "org")]
        scope: String,
        /// Print every decision (what it would end, link and alias, with the facts and the model's reason) as
        /// JSON lines, and commit nothing
        #[arg(long)]
        dry_run: bool,
        /// Most model calls this pass (else MEMSTORE_DREAM_CALLS, default 100)
        #[arg(long)]
        calls: Option<usize>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Where a subject stands, as a caller reads it: scores, gaps, items, current facts, each cited
    Brief {
        scope: String,
        /// Who reads it, as `query` takes it
        user: String,
        subject: String,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Compile the scope's skill library and their AIP step graphs (which subsumes corpus-to-skill) from the
    /// compile sources it hasn't yet, on a branch; `memstore serve` does this on its own as sources come in.
    /// Uses MEMSTORE_COMPILE (or MEMSTORE_EXTRACT; default OpenRouter, Claude Opus 5.5)
    Compile {
        scope: String,
        /// Source prefixes to compile (`kb:,file:`), or `*` for every source; else the scope's
        /// `compile_sources` setting, else MEMSTORE_COMPILE_SOURCES
        #[arg(long)]
        sources: Option<String>,
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
        /// At most this many sources this time
        #[arg(long, default_value_t = 100_000)]
        limit: usize,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Read the history the scope schema's `extract` section names with what the schema offers now: episodes
    /// no version of it read are read whole, those an earlier version read are asked only what was added since.
    /// Newest first, resumable; a second run reads nothing. Uses MEMSTORE_EXTRACT;
    /// MEMSTORE_EXTRACT_PRICE=<$/M in>,<$/M out> turns tokens into dollars
    Backfill {
        scope: String,
        /// Count the episodes and estimate tokens and dollars; read nothing
        #[arg(long)]
        dry_run: bool,
        /// At most this many episodes this run
        #[arg(long, default_value_t = 1000)]
        max_episodes: usize,
        /// Stop once this many dollars are spent (needs MEMSTORE_EXTRACT_PRICE)
        #[arg(long)]
        max_usd: Option<f64>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Learn advice from the scope's rewarded runs (`record_run`, `reward_run`): sort them into situations,
    /// then write a note per situation with enough runs that went wrong, served by `get_advice`
    Advice {
        scope: String,
        /// Who may read the advice, as ACL entries (`*`: everyone in the scope)
        #[arg(long, value_delimiter = ',', default_value = "*")]
        acl: Vec<String>,
        /// Runs that went wrong a situation needs before it gets a note
        #[arg(long, default_value_t = 2)]
        min_wrong: usize,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Runs for advice to learn from: `runs load` records a file of them, scored or not, in one batch
    #[command(subcommand)]
    Runs(RunsCmd),
    /// The training log (`MEMSTORE_TRAIN_LOG`): every model call kept whole, so a smaller model can be trained on
    /// it. `sweep` removes what the memory has since forgotten; `export` writes training examples
    #[command(subcommand)]
    TrainLog(TrainLogCmd),
    /// The answer log: what agents asked this memory and the facts their answers cited (`log_answer`), newest
    /// first, as JSON lines
    QaLog {
        scope: String,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Take back facts, for every reader: one by id, or every commit an extractor made so far (an actor prefix
    /// from the ledger, such as extract:<model>@g1/schema:<name>@v2, or a dreamer's, dream:<model>@s2). A withdrawn
    /// extractor's episodes are read again by the next extraction, summaries citing its facts are redone by the next
    /// aggregation, and a withdrawn dreamer's endings, links and aliases are gone
    RetractFacts {
        scope: String,
        #[arg(
            long,
            conflicts_with = "extractor",
            required_unless_present = "extractor"
        )]
        fact: Option<String>,
        #[arg(long)]
        extractor: Option<String>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Merge one subject into another, for every reader: its facts move under the survivor with their sources,
    /// refs to it now name the survivor, its name becomes the survivor's alias, and the old rows are retracted.
    /// One pair on the command line, or many from a file of `from<TAB>into` lines
    MergeSubjects {
        scope: String,
        /// The subject that goes (topic:avatar_voice_ui)
        #[arg(required_unless_present = "plan", conflicts_with = "plan")]
        from: Option<String>,
        /// The subject that stays (topic:avatar_voice_selector)
        #[arg(required_unless_present = "plan", conflicts_with = "plan")]
        into: Option<String>,
        /// A file of `from<TAB>into` lines, merged in order
        #[arg(long)]
        plan: Option<PathBuf>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Summarise what the schema's extracted facts add up to, per subject (item statuses, element scores,
    /// flags), as its `extract.aggregates` says
    Aggregate {
        #[arg(default_value = "org")]
        scope: String,
        /// Every subject now, not only those with a full batch or a long wait
        #[arg(long)]
        now: bool,
        /// Only this subject (deal:hs-1234567890)
        #[arg(long)]
        subject: Option<String>,
        /// Print what the run would write, one JSON line per subject, and commit nothing
        #[arg(long)]
        dry_run: bool,
        /// Print the subject's current aggregates instead (needs --subject)
        #[arg(long)]
        show: bool,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Fill missing fact quotes from their source episodes
    BackfillQuotes {
        scope: String,
        /// Recompute quotes that are already there
        #[arg(long, env = "MEMSTORE_BACKFILL_REDO")]
        redo: bool,
    },
    /// Load the embedding model and embed some text (checks an image's baked-in model)
    Embed {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Datasets in GCS (MEMSTORE_DATASETS): list, fetch on demand, publish
    #[command(subcommand)]
    Dataset(DatasetCmd),
    /// Engine checks (spec M7)
    #[command(subcommand)]
    Check(CheckCmd),
    /// Workspaces: isolated, disposable memories, served side by side and read pinned to a snapshot
    #[command(subcommand)]
    Workspace(WorkspaceCmd),
    #[command(flatten)]
    People(memstore::people::cli::PeopleCmd),
    /// The control plane's API: orgs, deployments, desired versions, members, audit, releases (docs/cloud-cli.md)
    #[cfg(feature = "control-plane")]
    Cloud(memstore::cloud::CloudArgs),
}

#[derive(Subcommand)]
enum PreflightCmd {
    /// A GCP project: run it as the project's admin, from Cloud Shell or any shell with gcloud signed in
    Gcp {
        /// The project memstore will run in
        #[arg(long)]
        project: String,
        /// The region
        #[arg(long, default_value = "us-central1")]
        region: String,
        /// Which IAM template you will apply: byoc (the vendor builds the network) or byovpc (you bring a subnet)
        #[arg(long, value_enum, default_value = "byoc")]
        variant: memstore::preflight::Variant,
        /// Where the engine runs
        #[arg(long, value_enum, default_value = "cloud-run")]
        runtime: memstore::preflight::Runtime,
        /// The templates' `name`: the prefix of every resource
        #[arg(long, default_value = memstore::provision::NAME)]
        name: String,
        /// The memory bucket the template will create (default <project>-memstore)
        #[arg(long)]
        bucket: Option<String>,
        /// The vendor's GCP customer id (C0…), from the guide, for the domain-restricted-sharing check
        #[arg(long, value_name = "C0...")]
        vendor_customer_id: Option<String>,
        /// The control plane the agent will reach, for the egress check
        #[arg(long, env = "MEMSTORE_CONTROL_PLANE_URL")]
        control_plane: Option<String>,
        /// The image registry host the nodes will pull from, for the egress check
        #[arg(long, value_name = "HOST")]
        image_registry: Option<String>,
        /// Check the running install instead: the engine, its IAM, health, image signature, the data stores
        /// and the enrollment
        #[arg(long)]
        installed: bool,
        /// With --installed: the vendor's cosign public key (PEM), to verify the running image's signature
        #[arg(long, value_name = "FILE", requires = "installed")]
        cosign_key: Option<PathBuf>,
        /// With --installed: the deployment credential (default ~/.memstore/deployment.json)
        #[arg(long, value_name = "FILE", requires = "installed")]
        credential: Option<PathBuf>,
        /// With --installed: where the image signatures live when not beside the image (the agent's
        /// MEMSTORE_AGENT_SIGNATURE_REPOSITORY)
        #[arg(long, value_name = "REPOSITORY", requires = "cosign_key")]
        signature_repository: Option<String>,
        /// With --installed: verify this image (repository@sha256:…) instead of the one the engine runs
        #[arg(long, value_name = "REPO@DIGEST", requires = "cosign_key")]
        image: Option<String>,
        /// One JSON object on stdout instead of the report (commands aren't echoed)
        #[arg(long)]
        json: bool,
        /// Print what each check reads, when it fails and its fix, as Markdown, and check nothing
        #[arg(long)]
        explain: bool,
    },
}

#[derive(Subcommand)]
enum EpisodeTypeCmd {
    /// Every episode type: its format, the sources it claims, its catalog's size
    List,
    /// One type's catalog (checked like a schema), its item types and the predicates the engine writes
    Show { name: String },
    /// Split a body file into the units a model reads and facts cite; with --quote, the unit a quote cites
    Units {
        /// The episode type, or `auto` to pick it from --source
        name: String,
        file: String,
        /// The episode's source (used by `auto`)
        #[arg(long, default_value = "")]
        source: String,
        #[arg(long)]
        quote: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum SchemaCmd {
    /// The built-in schemas
    List,
    /// A scope's schema on a branch
    Get {
        scope: String,
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Commit a scope's schema (a built-in name or a JSON file), its `extract` section included
    Set {
        scope: String,
        schema: String,
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
        /// Allow a breaking change on main (the version must also be higher)
        #[arg(long)]
        breaking: bool,
        /// Make every check the set would (the breaking-change check too) and commit nothing
        #[arg(long)]
        dry_run: bool,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Check a schema (a built-in name or a JSON file) without a catalog; prints its summary
    Check { schema: String },
}

#[derive(Subcommand)]
enum DeploymentCmd {
    /// Trade a one-time enrollment token for this install's id; the new private key stays here
    Enroll {
        control_plane: String,
        token: String,
        /// Where to keep the credential (MEMSTORE_DEPLOYMENT_CREDENTIAL, else ~/.memstore/deployment.json)
        #[arg(long)]
        credential: Option<PathBuf>,
    },
    /// Replace the key, registering the new one with a request the old one signs
    Rotate {
        #[arg(long)]
        credential: Option<PathBuf>,
    },
    /// Send payloads (JSON lines, `-` for stdin; what `GET /telemetry` shows) to the control plane, signed
    Send {
        file: String,
        #[arg(long)]
        credential: Option<PathBuf>,
    },
    /// Ask the control plane who this install is (a signed request)
    Whoami {
        #[arg(long)]
        credential: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct AgentArgs {
    /// The deployment credential (MEMSTORE_DEPLOYMENT_CREDENTIAL, else ~/.memstore/deployment.json)
    #[arg(long)]
    credential: Option<PathBuf>,
    /// Where the agent keeps the generation it applied and the actions it ran (default: beside the
    /// telemetry spool, `agent.state.json`)
    #[arg(long, env = "MEMSTORE_AGENT_STATE")]
    state: Option<PathBuf>,
    /// The engine's telemetry mirror, on a volume shared with it: lets drain_spool and clear_spool act
    #[arg(long, env = "MEMSTORE_TELEMETRY_MIRROR")]
    telemetry_mirror: Option<PathBuf>,
    /// The control plane's public key (base64), when the credential doesn't pin it
    #[arg(long, env = "MEMSTORE_CONTROL_PLANE_KEY")]
    control_plane_key: Option<String>,
    /// With no key pinned (credential or --control-plane-key), trust the one /v1/whoami gives on first
    /// use and keep it; its fingerprint is logged for you to check
    #[arg(long, env = "MEMSTORE_AGENT_TOFU", action = clap::ArgAction::SetTrue,
          value_parser = clap::builder::BoolishValueParser::new())]
    trust_control_plane_key_on_first_use: bool,
    /// Where the engine runs: kubernetes (cloud-run and snowflake are not built yet)
    #[arg(long, env = "MEMSTORE_AGENT_TARGET", default_value = "kubernetes")]
    target: String,
    /// The image repository the engine runs from; the control plane names only a tag and a digest
    #[arg(long, env = "MEMSTORE_AGENT_IMAGE_REPOSITORY")]
    image_repository: String,
    /// The embeddings backends the control plane may switch the engine to (comma-separated: local, openai,
    /// openrouter). None by default: a switch to an API sends the deployment's text to that vendor
    #[arg(long, env = "MEMSTORE_AGENT_EMBED_PROVIDERS", value_delimiter = ',')]
    embed_providers: Vec<String>,
    /// Report workload drift, or repair it within maintenance windows (a local operator choice)
    #[arg(
        long,
        env = "MEMSTORE_AGENT_DRIFT_MODE",
        value_enum,
        default_value = "report"
    )]
    drift_mode: memstore::agent::DriftMode,
    /// The engine's namespace (default: the agent's own)
    #[arg(long, env = "MEMSTORE_AGENT_NAMESPACE")]
    namespace: Option<String>,
    /// The engine's Deployment
    #[arg(long, env = "MEMSTORE_AGENT_DEPLOYMENT", default_value = "memstore")]
    deployment: String,
    /// The engine's container in it
    #[arg(long, env = "MEMSTORE_AGENT_CONTAINER", default_value = "memstore")]
    container: String,
    /// More Deployments running the same engine (cells), given the same version and flags (comma-separated)
    #[arg(long, env = "MEMSTORE_AGENT_ALSO_DEPLOYMENTS", value_delimiter = ',')]
    also_deployments: Vec<String>,
    /// The Kubernetes API, instead of the in-cluster one (https, or http to this machine only)
    #[arg(long, env = "MEMSTORE_AGENT_KUBE_API", hide = true)]
    kube_api: Option<String>,
    /// The engine's health: `/healthz`, or `/telemetry` with --health-token-file (default:
    /// http://<deployment>.<namespace>.svc:8080/healthz)
    #[arg(long, env = "MEMSTORE_AGENT_HEALTH_URL")]
    health_url: Option<String>,
    /// An admin key for --health-url, read from this file
    #[arg(long, env = "MEMSTORE_AGENT_HEALTH_TOKEN_FILE")]
    health_token_file: Option<PathBuf>,
    /// The engine, for support diagnostics (default: the origin of --health-url)
    #[arg(long, env = "MEMSTORE_AGENT_ENGINE_URL")]
    engine_url: Option<String>,
    /// The operator's admin key for the engine, read from this file: support diagnostics read
    /// /telemetry and /metrics with it, and support sessions are written to the engine's audit with it
    #[arg(long, env = "MEMSTORE_AGENT_ADMIN_KEY_FILE")]
    admin_key_file: Option<PathBuf>,
    /// Seconds between looks at an open support session
    #[arg(long, env = "MEMSTORE_AGENT_SUPPORT_POLL_SECONDS", default_value_t = 5)]
    support_poll_seconds: u64,
    /// Seconds between rounds (each give or take a fifth)
    #[arg(long, env = "MEMSTORE_AGENT_POLL_SECONDS", default_value_t = 30)]
    poll_seconds: u64,
    /// Seconds a rollout may take before it is rolled back. Longer than a rollout Kubernetes still counts as
    /// progressing (`progressDeadlineSeconds`, e.g. 900): an engine is ready only once the ledger has
    /// loaded (minutes cold) and the rest has warmed or `MEMSTORE_READY_TIMEOUT_SECS` (300) has passed, and
    /// a stall is caught as `rollout_stalled` anyway
    #[arg(
        long,
        env = "MEMSTORE_AGENT_ROLLOUT_TIMEOUT_SECONDS",
        default_value_t = 1200
    )]
    rollout_timeout_seconds: u64,
    /// Seconds a rolled-out engine has to answer healthy before it is rolled back
    #[arg(
        long,
        env = "MEMSTORE_AGENT_HEALTH_TIMEOUT_SECONDS",
        default_value_t = 120
    )]
    health_timeout_seconds: u64,
    /// The PEM public key (P-256) an image's cosign signature must verify with before it is run
    #[arg(long, env = "MEMSTORE_AGENT_COSIGN_KEY")]
    cosign_key: Option<PathBuf>,
    /// With no --cosign-key, run images with no signature check (warned of at start and each upgrade).
    /// Without either, no upgrade is applied
    #[arg(long, env = "MEMSTORE_AGENT_ALLOW_UNSIGNED", action = clap::ArgAction::SetTrue,
          value_parser = clap::builder::BoolishValueParser::new())]
    allow_unsigned_images: bool,
    /// How signatures are read from the registry: gcp-metadata, anonymous or none (default:
    /// gcp-metadata for *-docker.pkg.dev and gcr.io, else anonymous)
    #[arg(long, env = "MEMSTORE_AGENT_REGISTRY_AUTH")]
    registry_auth: Option<String>,
    /// The registry's API, instead of https://<the repository's host> (https, or http to this machine)
    #[arg(long, env = "MEMSTORE_AGENT_REGISTRY_URL", hide = true)]
    registry_url: Option<String>,
    /// The GCE metadata server's host
    #[arg(long, env = "GCE_METADATA_HOST", hide = true)]
    metadata_host: Option<String>,
    /// The repository the images' cosign signatures are in, when they were signed with
    /// COSIGN_REPOSITORY (default: the image repository)
    #[arg(long, env = "MEMSTORE_AGENT_SIGNATURE_REPOSITORY")]
    signature_repository: Option<String>,
    /// One round, then exit
    #[arg(long)]
    once: bool,
}

#[derive(Subcommand)]
enum TelemetryCmd {
    /// The contract's JSON schema: every field a payload may hold
    Schema,
    /// Check payloads (JSON lines, `-` for stdin) against the contract: refuses any field it lacks
    Check { file: String },
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct IndexArgs {
    #[command(subcommand)]
    cmd: Option<IndexCmd>,
    /// The scope to bring up to date
    scope: Option<String>,
    /// Don't update: wait until the indexes cover the head (as a server updates them)
    #[arg(long)]
    wait: bool,
    /// With --wait: this branch's head only
    #[arg(long)]
    branch: Option<String>,
    /// With --wait: give up after this many seconds
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
    #[arg(long)]
    local: Option<PathBuf>,
}

#[derive(Subcommand)]
enum IndexCmd {
    /// The heads each index covers, the scope's heads, the tail not indexed yet, and the configuration
    Status { scope: String },
    /// Build the indexes from the tables into an empty location
    Rebuild {
        scope: String,
        #[arg(long)]
        to: String,
    },
    /// Compare the incremental indexes with a rebuild (into a temporary location, unless --against one);
    /// fails on any difference
    Verify {
        scope: String,
        #[arg(long)]
        against: Option<String>,
    },
    /// Every index of the scope in the index location, with its configuration and size
    List { scope: String },
    /// Delete an index of another configuration (a directory `index list` shows)
    Drop { scope: String, dir: String },
}

#[derive(Clone, Subcommand)]
enum RunsCmd {
    /// Record runs from a JSON-lines file, one `{task, trace, used?, outcome?, advice?, reward?, reason?}` a line
    /// (the items `POST /runs {runs: [...]}` takes; a `goal` is accepted and ignored), as one batch: every line
    /// is checked before any is written, then the traces go in one commit and the answer-log entries, with their
    /// rewards, in another.
    /// Prints the run ids in the file's order. The way to load historical runs whose outcomes are known
    Load {
        scope: String,
        file: PathBuf,
        /// The person the runs are, `user` (or `user:group,...`, the groups the facts they used are read as):
        /// their traces are readable by them alone. Needed unless --local, whose runs are by default its one
        /// caller's, as `serve --local` records them
        #[arg(long = "as", value_name = "USER")]
        as_user: Option<String>,
        /// With --local and no --as: the groups `serve --local` was given (default `local`)
        #[arg(long, value_delimiter = ',')]
        groups: Vec<String>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
}

#[derive(Clone, Subcommand)]
enum TrainLogCmd {
    /// Remove from the log the calls that read an episode or a run the memory has since forgotten (offboarded,
    /// retained out, retracted, its ACL narrowed to no one). A server does this on a timer; this is the same, now
    Sweep {
        /// Only this org's calls (every org's by default)
        #[arg(long)]
        org: Option<String>,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Write a scope's logged calls as JSON-lines training examples: the prompt and the reply to learn, what the
    /// call read, its outcomes, what later came of it (a note's served runs' rewards), and a train/dev/test split
    /// by what it read. Only calls whose reply was whole and parsed, whose work did not error and whose sources
    /// are still there; and only episodes shared with `*`, unless --include-restricted
    Export {
        scope: String,
        /// The file to write
        #[arg(long)]
        out: PathBuf,
        /// Also calls that read what not every reader of the org may read (an episode with a narrower ACL, a
        /// run's trace, a session): for a model only those readers will use
        #[arg(long)]
        include_restricted: bool,
        /// A `serve --local` directory, instead of the catalog the environment names (stop the server first)
        #[arg(long)]
        local: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// A new workspace: an org of its own, with its scope's tables, its schema (if given) and its limits
    Create {
        ws: String,
        /// A built-in schema's name or a JSON file, committed on main
        #[arg(long)]
        schema: Option<String>,
        /// Delete it with `workspace gc` after this long (30m, 12h, 7d); it isn't served after it
        #[arg(long)]
        ttl: Option<String>,
        #[arg(long)]
        max_episodes: Option<u64>,
        #[arg(long)]
        max_facts: Option<u64>,
        #[arg(long)]
        max_index_bytes: Option<u64>,
    },
    /// Every workspace
    List,
    /// A workspace's scopes, branches, row counts, index and snapshots
    Show { ws: String },
    /// A branch of the workspace from main as it is now (an arm that shares main's sources)
    Branch { ws: String, branch: String },
    /// Record the heads of every scope and branch; prints the snapshot id reads pin to (at=)
    Snapshot {
        ws: String,
        #[arg(long)]
        label: Option<String>,
        /// The branch reads pinned to this snapshot read unless they name another
        #[arg(long, default_value = memstore::MAIN)]
        branch: String,
    },
    /// Delete a workspace for good: its tables, ledger rows, search indexes and snapshots
    Delete {
        ws: String,
        /// Repeat the name
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Delete every workspace past its time to live
    Gc,
}

#[derive(Subcommand)]
enum DatasetCmd {
    /// Every dataset and its latest version
    List,
    /// Fetch `name` or `name@version`; files already present with the right hash are kept
    Get {
        spec: String,
        /// Where to put it (default corpora/<name>)
        dir: Option<PathBuf>,
    },
    /// Upload a directory as a new version and make it the latest
    Put {
        name: String,
        dir: PathBuf,
        /// Default: the current UTC time, YYYYMMDD-HHMM
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
}

#[derive(Subcommand, Clone)]
enum CheckCmd {
    /// Many writers commit at once: no lost or double commits, one ledger chain
    Concurrent {
        scope: String,
        writers: usize,
        commits: usize,
        /// Writers share one catalog client
        #[arg(long)]
        shared: bool,
    },
    /// A pinned session's reads don't change while writers commit
    Pinning { scope: String, writes: usize },
    /// Seeded crashes before the ledger append leave nothing visible
    Crash {
        scope: String,
        crashes: usize,
        seed: u64,
    },
    /// Random branch and merge sequences match an in-memory model
    Model {
        sequences: usize,
        ops: usize,
        seed: u64,
    },
    /// Writers with skewed clocks keep recorded time increasing along the ledger
    Skew {
        scope: String,
        writers: usize,
        commits: usize,
        skew_secs: u64,
    },
    /// Episodes written by more than one source (the same text remembered twice before PR 53): each one's
    /// rows, so an operator can re-ACL any that a later copy widened
    Collisions {
        #[arg(default_value = "org")]
        scope: String,
        /// A `serve --local` directory, instead of the catalog the environment names
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// A write becomes visible to another warm reader within NFR-3's 5 minutes
    Freshness { scope: String, samples: usize },
    /// Commits per second and facts per second
    Throughput {
        scope: String,
        writers: usize,
        commits: usize,
        facts: usize,
        #[arg(long)]
        shared: bool,
    },
    /// An index updated batch by batch equals a full rebuild
    IndexEquivalence {
        scope: String,
        batches: usize,
        per_batch: usize,
        seed: u64,
    },
    /// Sampled facts are visible exactly to the principals that may see their sources
    SourceAcl {
        dir: PathBuf,
        sample: usize,
        seed: u64,
    },
}

/// jemalloc, not glibc's malloc: glibc keeps freed memory in its per-thread arenas (one deployment's server held
/// 0.7 to 1 GB "free but held" with `malloc_trim` every 5 minutes), where jemalloc returns it as it decays.
/// Not in a dylib build: the dependencies' dylib allocates with the system allocator, and memory one allocator
/// hands out and the other frees aborts the process at startup.
#[cfg(not(feature = "dylib"))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc's counters for the memory log: what is allocated, what its pages hold resident, and the
/// difference, the freed memory it hasn't returned yet.
#[cfg(not(feature = "dylib"))]
fn jemalloc_stats() -> String {
    use tikv_jemalloc_ctl::{epoch, stats};
    let _ = epoch::advance();
    let (Ok(allocated), Ok(resident), Ok(retained)) = (
        stats::allocated::read(),
        stats::resident::read(),
        stats::retained::read(),
    ) else {
        return ", jemalloc stats unreadable".into();
    };
    format!(
        ", jemalloc allocated {} MB, resident {} MB, free but held {} MB, retained (unmapped) {} MB",
        allocated >> 20,
        resident >> 20,
        resident.saturating_sub(allocated) >> 20,
        retained >> 20
    )
}

fn main() -> Result<()> {
    // Lance scores every index search (keyword, vector, fact and passage lists) on one pool of the CPUs it counts
    // less 2 it holds back for I/O (`LANCE_IO_CORE_RESERVATION`), which memstore's I/O, on its own runtime, doesn't
    // use: 2 threads in a pod limited to 3.5 CPUs, 1 in a smaller one. A search's four to six lists queued for
    // them, so its vector lists took as long as its slowest keyword list, about 2 s, while the pod used 2 of its
    // CPUs, unthrottled. The pool gets every CPU the process may use, unless either is set.
    if std::env::var_os("LANCE_CPU_THREADS").is_none()
        && std::env::var_os("LANCE_IO_CORE_RESERVATION").is_none()
    {
        // SAFETY: nothing else runs yet (the runtime and every thread start below), so no read races this write.
        unsafe { std::env::set_var("LANCE_IO_CORE_RESERVATION", "0") };
    }
    #[cfg(not(feature = "dylib"))]
    let _ = memstore::metrics::ALLOCATOR_STATS.set(jemalloc_stats);
    let cli = Cli::parse();
    if let Some(c) = &cli.context {
        memstore::people::client::override_context(c);
    }
    // Errors to Sentry from the long-running processes, when the environment says so (src/report.rs); kept for the
    // process's life, flushing on the way out.
    let _sentry = match cli.command {
        Cmd::Serve { .. } | Cmd::Agent(_) => memstore::report::init(),
        _ => None,
    };
    // Debug builds poll deep futures (nested query sub-queries over Lance scans, the MCP handler's tool calls) with
    // large frames; tokio's default 2 MiB worker stacks are too small for them, and in a debug build 8 MiB is too:
    // a session tool's first call overflowed it. Reserved, not committed, so the debug size costs nothing idle.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(if cfg!(debug_assertions) {
            32 << 20
        } else {
            8 << 20
        })
        .build()?;
    // The command itself is polled by the thread that blocks on it, not a worker: in a debug build its frames (the
    // command's own, then a compile's reading the catalog) outgrow the 8 MiB a process's main thread gets, and
    // `goal compile` overflowed it. So it blocks on a thread of its own, with room to spare.
    let done = std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("main".into())
            .stack_size(64 << 20)
            .spawn_scoped(s, || rt.block_on(Box::pin(run(cli.command, cli.direct))))
            .map(|t| t.join())
    });
    let done = match done {
        Ok(Ok(done)) => done,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(e) => Err(anyhow::Error::from(e).context("a thread for the command")),
    };
    // Dropping the runtime would wait for every blocking thread still running, however long: a command that is
    // done (a server told to stop) must not be kept alive by work no one waits for. Ten seconds lets a short
    // one (a file write) finish.
    rt.shutdown_timeout(std::time::Duration::from_secs(10));
    // A server or agent that stops on an error (a bad setting at start, say) is a failure too.
    if let Err(e) = &done {
        memstore::report::failure("exit", e);
    }
    done
}

/// What to say when no model is configured for extraction (and, through it, aggregation and compiling).
const NO_MODEL: &str = "set OPENROUTER_API_KEY for the default, OpenRouter with Claude Haiku 5.5 (or the model \
MEMSTORE_EXTRACT_MODEL names), or set MEMSTORE_EXTRACT (openrouter, openrouter-batch, openai, anthropic, bedrock, vertex or cortex)";

/// `memstore qa-log`: the answer log, as JSON lines.
/// `memstore advice`.
/// `memstore schema get`: the scope's schema on `branch`.
async fn schema_get(store: &Store, scope: &str, branch: &str) -> Result<()> {
    match store.schema(&Scope::parse(scope)?, branch).await? {
        Some(s) => println!("{}", serde_json::to_string_pretty(&*s)?),
        None => println!("{scope} has no schema (untyped)"),
    }
    Ok(())
}

/// `memstore schema set`: commit `schema` (a built-in name or a file) as the scope's on `branch`; with
/// `dry_run`, make the same checks and commit nothing.
async fn schema_set(
    store: &Store,
    scope: &str,
    schema: &str,
    branch: &str,
    breaking: bool,
    dry_run: bool,
) -> Result<()> {
    let s = memstore::query::schema::Schema::load(schema)?;
    if dry_run {
        let was = store
            .check_schema(&Scope::parse(scope)?, branch, &s, breaking)
            .await?;
        let was = was.map_or(String::new(), |w| {
            format!(" (was {} v{})", w.name, w.version)
        });
        println!(
            "would: {scope} on {branch} uses schema {} v{}{was}",
            s.name, s.version
        );
        return Ok(());
    }
    let id = store
        .set_schema(&Scope::parse(scope)?, branch, "cli", &s, breaking)
        .await?;
    println!(
        "{scope} on {branch} now uses schema {} v{} (commit {id})",
        s.name, s.version
    );
    Ok(())
}

/// `memstore compile`: the scope's skill library from `sources` (else the scope's own compile sources) on `branch`.
async fn compile(
    store: &Store,
    scope: &str,
    sources: Option<&str>,
    branch: &str,
    limit: usize,
) -> Result<()> {
    let cx = memstore::extract::Extractor::compile_from_env()?.ok_or_else(|| {
        anyhow::anyhow!(
            "no compile model: {}",
            NO_MODEL.replacen(
                "set MEMSTORE_EXTRACT (",
                "set MEMSTORE_COMPILE or MEMSTORE_EXTRACT (",
                1
            )
        )
    })?;
    let scope_ = Scope::parse(scope)?;
    let dir = memstore::people::directory::Directory::load(store)
        .await
        .ok();
    let sources = match sources {
        Some(s) => memstore::compile::parse_sources(s),
        None => memstore::compile::sources_for(dir.as_ref(), &scope_),
    };
    // The scope's batch size and what its sources are, as its settings say, as `serve` compiles it.
    let opts = memstore::compile::options_for(dir.as_ref(), &scope_);
    anyhow::ensure!(
        !sources.is_empty(),
        "{scope} has no compile sources: pass --sources kb:,file: (or * for every source), or set the org's \
         compile_sources"
    );
    let r =
        memstore::compile::run_with(store, &scope_, branch, &sources, &cx, &opts, limit).await?;
    println!(
        "{scope} on {branch}: {} with {}: {} sources compiled ({} not compile sources), {} skills and {} AIP graphs written, {} dropped, {} failed ({} with the API down); {}",
        sources.join(","),
        cx.model(),
        r.episodes,
        r.skipped,
        r.skills,
        r.aip,
        r.dropped,
        r.failed,
        r.unavailable,
        cx.calls()
    );
    anyhow::ensure!(
        r.unavailable == 0,
        "the compile API was down for {} sources; run again",
        r.unavailable
    );
    Ok(())
}

async fn advice(store: &Store, scope: &str, acl: &[String], min_wrong: usize) -> Result<()> {
    let cx = memstore::extract::Extractor::advice_from_env()?.ok_or_else(|| {
        anyhow::anyhow!(
            "no model to learn advice with: set MEMSTORE_ADVICE_MODEL (or OPENROUTER_API_KEY)"
        )
    })?;
    let done = memstore::advice::learn(store, &Scope::parse(scope)?, &cx, acl, min_wrong).await?;
    let (tin, tout) = cx.usage();
    let calls = cx.calls();
    // Tokens paid for, and those of the replies the model-call cache gave (paid for once, by an earlier build).
    println!(
        "{}",
        serde_json::json!({"learned": done,
            "tokens": {"in": tin, "out": tout, "cached": {"in": calls.cached_in, "out": calls.cached_out}},
            "calls": {"paid": calls.paid, "cached": calls.cached}})
    );
    Ok(())
}

/// `memstore runs load`: the file as one batch ([`memstore::runs::load`]); a refused line is named by its number.
async fn runs(store: &Store, cmd: RunsCmd) -> Result<()> {
    let RunsCmd::Load {
        scope,
        file,
        as_user,
        groups,
        local,
    } = cmd;
    let person = match (&as_user, &local) {
        (Some(u), _) => Some(principal(u)),
        (None, Some(_)) => None,
        (None, None) => anyhow::bail!(
            "runs are a person's: name whose with --as <user> (only a --local store has a caller of its own)"
        ),
    };
    let groups = if groups.is_empty() {
        vec!["local".to_string()]
    } else {
        groups
    };
    let text =
        std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let (mut items, mut lines) = (vec![], vec![]);
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("{} line {}: not JSON: {e}", file.display(), n + 1))?;
        anyhow::ensure!(
            v.is_object(),
            "{} line {}: a run is a JSON object",
            file.display(),
            n + 1
        );
        items.push(v);
        lines.push(n + 1);
    }
    anyhow::ensure!(!items.is_empty(), "{}: no runs", file.display());
    let t = std::time::Instant::now();
    let rewarded = items.iter().filter(|v| !v["reward"].is_null()).count();
    let ids = memstore::runs::load(
        store,
        &Scope::parse(&scope)?,
        person.as_ref(),
        &groups,
        &items,
    )
    .await
    .map_err(|e| match e.downcast_ref::<memstore::runs::Refused>() {
        Some(r) => {
            let at =
                r.0.strip_prefix("runs[")
                    .and_then(|rest| rest.split_once(']'))
                    .and_then(|(i, _)| i.parse::<usize>().ok())
                    .and_then(|i| lines.get(i));
            match at {
                Some(n) => anyhow::anyhow!("{} line {n}: {}", file.display(), r.reason()),
                None => anyhow::anyhow!("{r}"),
            }
        }
        None => e,
    })?;
    for id in &ids {
        println!("{id}");
    }
    eprintln!(
        "recorded {} runs ({rewarded} with rewards) in {scope} in {:.1}s",
        ids.len(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn maintain(store: &Store, scope: &str, keep: usize) -> Result<()> {
    for (table, before, after) in store.maintain(&Scope::parse(scope)?, keep).await? {
        println!("{table}: {before} -> {after} snapshots");
    }
    Ok(())
}

async fn train_log(store: &Store, cmd: TrainLogCmd) -> Result<()> {
    match cmd {
        TrainLogCmd::Sweep { org, .. } => {
            let done = memstore::train_data::sweep(store, org.as_deref()).await?;
            println!("{}", serde_json::to_string(&done)?);
        }
        TrainLogCmd::Export {
            scope,
            out,
            include_restricted,
            ..
        } => {
            let file = std::fs::File::create(&out)?;
            let mut file = std::io::BufWriter::new(file);
            let done = memstore::train_data::export(
                store,
                &Scope::parse(&scope)?,
                include_restricted,
                &mut file,
            )
            .await?;
            std::io::Write::flush(&mut file)?;
            println!("{}", serde_json::to_string(&done)?);
        }
    }
    Ok(())
}

async fn qa_log(store: &Store, scope: &str) -> Result<()> {
    for l in memstore::qa_log::list(store, &Scope::parse(scope)?, None).await? {
        println!("{}", serde_json::to_string(&l)?);
    }
    Ok(())
}

/// MEMSTORE_EXTRACT_PRICE: dollars per million input and output tokens (`3,15`), if set.
fn extract_price() -> Result<Option<(f64, f64)>> {
    match std::env::var("MEMSTORE_EXTRACT_PRICE") {
        Ok(p) if !p.trim().is_empty() => {
            let (i, o) = p.split_once(',').ok_or_else(|| {
                anyhow::anyhow!("MEMSTORE_EXTRACT_PRICE is <$ per M input>,<$ per M output>")
            })?;
            Ok(Some((i.trim().parse::<f64>()?, o.trim().parse::<f64>()?)))
        }
        _ => Ok(None),
    }
}

/// `memstore compact-ledger`: each step on stderr as it goes; the rows and their checksum before and after, and the
/// summary `serve`'s timer logs (counts only), on stdout. `--dry-run`: the ledger's files now and what a compaction
/// would leave, nothing written. `--aggregates <scope>`: the same for that scope's summaries.
async fn compact_ledger(store: &Store, dry_run: bool, aggregates: Option<&str>) -> Result<()> {
    let started = std::time::Instant::now();
    let scope = aggregates.map(Scope::parse).transpose()?;
    let (table, nothing) = match &scope {
        Some(s) => (
            format!("{}.{}", s.namespace(), memstore::AGGREGATES),
            format!("no summaries in {s} to compact"),
        ),
        None => (
            "memory_system.ledger".to_string(),
            "no ledger to compact".to_string(),
        ),
    };
    let status = async || match &scope {
        Some(s) => store.aggregates_status(s).await,
        None => store.ledger_status().await,
    };
    let Some(before) = status().await? else {
        println!("{nothing}");
        return Ok(());
    };
    let interrupted = if before.unswept {
        "an earlier compaction committed and wasn't swept: this run finishes it first"
    } else if before.pending > 0 {
        "a pass of an earlier run never landed (or one is running now): its files go with a sweep once it is an \
         hour old"
    } else {
        "none"
    };
    if dry_run {
        let after_files = before.data_files_after();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "rows": before.rows,
                "bytes": before.bytes,
                "snapshots": before.snapshots,
                "data_files_now": before.data_files,
                "data_files_after": after_files,
                "metadata_files_now": before.metadata_files(),
                // One manifest of the new files, one manifest list, and the table's metadata files.
                "metadata_files_after": u64::from(after_files > 0) + 1 + before.metadata_logs as u64,
                "last_compacted": before.compacted_at_ms.map(|ms| memstore::audit::rfc3339(ms * 1000)),
                "interrupted_run": interrupted,
            }))?
        );
        println!(
            "would rewrite {} rows from {} data files into {}, then expire {} snapshots and delete the files only \
             they referenced; nothing was written",
            before.rows,
            before.data_files,
            before.data_files_after(),
            before.snapshots
        );
        return Ok(());
    }
    if interrupted != "none" {
        eprintln!("interrupted run: {interrupted}");
    }
    let progress = |step: &str| eprintln!("{step}");
    let compacted = match &scope {
        Some(s) => store.compact_aggregates(s, &progress).await?,
        None => store.compact_ledger(&progress).await?,
    };
    let Some(c) = compacted else {
        println!("{nothing}");
        return Ok(());
    };
    let took = started.elapsed();
    let after = status().await?.unwrap_or_default();
    println!(
        "before: {} in {} data files, {} snapshots",
        c.before, c.files_before, c.snapshots_before
    );
    println!("committed meanwhile: {}", c.late);
    println!("after:  {} in {} data files", c.after, c.files_after);
    if c.removed > 0 {
        println!("dropped: {} rows no reader is served", c.removed);
    }
    println!(
        "{}",
        serde_json::json!({
            "result": "ok",
            "rows": c.after.rows,
            "rows_dropped": c.removed,
            "rows_carried_over": c.late.rows,
            "data_files_before": before.data_files,
            "data_files_after": after.data_files,
            "metadata_files_before": before.metadata_files(),
            "metadata_files_after": after.metadata_files(),
            "bytes_before": before.bytes,
            "bytes_after": after.bytes,
            "duration_ms": took.as_millis() as u64,
            "swap_tries": c.swap_tries,
            "restarts": c.restarts,
        })
    );
    println!("compacted {table} in {:.1}s", took.as_secs_f64());
    Ok(())
}

/// `memstore retract-facts`: as the catalog's owner.
async fn retract_facts(
    store: &Store,
    scope: &str,
    fact: Option<&str>,
    extractor: Option<&str>,
) -> Result<()> {
    let actor = memstore::people::cli::owner().user;
    let n = store
        .retract_facts(&Scope::parse(scope)?, &actor, fact, extractor)
        .await?;
    println!("{scope}: {n} facts no reader sees any more");
    Ok(())
}

/// `memstore merge-subjects`: as the catalog's owner, one pair or a plan file's pairs in order.
async fn merge_subjects(
    store: &Store,
    scope: &str,
    from: Option<&str>,
    into: Option<&str>,
    plan: Option<&std::path::Path>,
) -> Result<()> {
    let actor = memstore::people::cli::owner().user;
    let scope = Scope::parse(scope)?;
    let pairs: Vec<(String, String)> = match plan {
        Some(p) => std::fs::read_to_string(p)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                l.split_once('\t')
                    .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
                    .ok_or_else(|| anyhow::anyhow!("not `from<TAB>into`: {l}"))
            })
            .collect::<Result<_>>()?,
        None => vec![(from.unwrap().to_string(), into.unwrap().to_string())],
    };
    for (from, into) in pairs {
        let n = store.merge_subjects(&scope, &actor, &from, &into).await?;
        println!("{from} -> {into}: {n} facts moved");
    }
    Ok(())
}

/// `memstore aggregate`: summarise due subjects (all with `now`), or with `dry_run` what a run would write, or
/// with `show` a subject's current aggregates as the operator reads them.
async fn aggregate(
    store: &Store,
    scope: &str,
    now: bool,
    subject: Option<&str>,
    dry_run: bool,
    show: bool,
) -> Result<()> {
    let scope = Scope::parse(scope)?;
    if show {
        let s = subject.ok_or_else(|| anyhow::anyhow!("--show needs --subject"))?;
        let operator = memstore::Principal::new("operator", &["*"]);
        let session = store.pin(&scope, memstore::MAIN).await?;
        for r in memstore::aggregate::current(&session, store, &operator, s).await? {
            println!("{}", serde_json::to_string(&r)?);
        }
        return Ok(());
    }
    let ex = memstore::extract::Extractor::from_env()?.ok_or_else(|| anyhow::anyhow!(NO_MODEL))?;
    let ex = match std::env::var("MEMSTORE_AGGREGATE_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
    {
        Some(m) => ex.with_model(&m),
        None => ex,
    };
    store.ensure_scope(&scope).await?;
    let when = match now {
        true => memstore::aggregate::When::All,
        false => memstore::aggregate::When::Due,
    };
    if dry_run {
        let mut report = vec![];
        let r =
            memstore::aggregate::run(store, &scope, &ex, when, subject, Some(&mut report)).await?;
        for line in report {
            println!("{line}");
        }
        eprintln!(
            "{}: dry run, nothing committed: {} subjects, {} rows",
            ex.model(),
            r.subjects,
            r.rows
        );
        return Ok(());
    }
    let r = memstore::aggregate::run(store, &scope, &ex, when, subject, None).await?;
    println!(
        "{}: {} subjects summarised, {} rows, {} waiting for a fuller batch, {} failed; {}",
        ex.model(),
        r.subjects,
        r.rows,
        r.waiting,
        r.failed,
        ex.calls()
    );
    Ok(())
}

/// `memstore extract`: one run, or with `dry_run` what a run would do.
async fn extract(
    store: &Store,
    scope: &str,
    branch: &str,
    limit: usize,
    dry_run: bool,
) -> Result<()> {
    let ex = memstore::extract::Extractor::from_env()?.ok_or_else(|| anyhow::anyhow!(NO_MODEL))?;
    let scope = Scope::parse(scope)?;
    store.ensure_scope(&scope).await?;
    let on_main = branch == memstore::MAIN;
    // Goal-blind extraction reads main only (it reconciles with main's facts): on a branch, goals alone.
    if !(on_main && memstore::extract::goal_blind_in(store, &scope).await)
        && memstore::extract::goals::contract_on(store, &scope, branch)
            .await?
            .is_none()
    {
        eprintln!(
            "{scope}'s schema on {branch} has no `extract` section, so nothing but its starter documents is extracted{}",
            if on_main {
                " (an org's admins' set_settings {extract: true}, or MEMSTORE_EXTRACT_GOAL_BLIND=1, extracts without one)"
            } else {
                " (goal-blind extraction reads main only)"
            }
        );
    }
    let r = if dry_run {
        let (r, report) = memstore::extract::dry_run_on(store, &scope, branch, &ex, limit).await?;
        for line in report {
            println!("{line}");
        }
        eprintln!(
            "{}: dry run, nothing committed: {} episodes would add {} facts, end {} and record {} in conflict",
            ex.id(),
            r.episodes,
            r.facts,
            r.closed,
            r.conflicts
        );
        return Ok(());
    } else {
        memstore::extract::run_on(store, &scope, branch, &ex, limit).await?
    };
    println!(
        "{}: read {} episodes, {} facts, ended {}, {} in conflict, {} failed (retried next run); {}",
        ex.id(),
        r.episodes,
        r.facts,
        r.closed,
        r.conflicts,
        r.failed,
        ex.calls()
    );
    Ok(())
}

/// `memstore dream`: one pass, or with `dry_run` what a pass would do.
async fn dream(store: &Store, scope: &str, dry_run: bool, calls: Option<usize>) -> Result<()> {
    let ex =
        memstore::extract::Extractor::dream_from_env()?.ok_or_else(|| anyhow::anyhow!(NO_MODEL))?;
    let scope = Scope::parse(scope)?;
    let mut budget = memstore::dream::Budget::from_env()?;
    if let Some(n) = calls {
        anyhow::ensure!(n > 0, "--calls is a whole number above 0");
        budget.calls = n;
    }
    // Each decision as its batch commits: a pass cut off midway has printed all it kept.
    let r = memstore::dream::pass_with(store, &scope, &ex, &budget, dry_run, &mut |d| {
        println!("{d}")
    })
    .await?;
    eprintln!(
        "{}{}: {} new facts; audiences {}; weighed {} parts of things and {} name clusters in {} calls ({} failed, {} \
         left for the next pass); {} ended, {} linked as duplicates, {} aliases, {} refused, {} released; {} parts \
         and names recorded as weighed, in {} batches; {}",
        r.dreamer,
        if dry_run {
            " (dry run, nothing committed)"
        } else {
            ""
        },
        r.new_facts,
        if r.audiences.is_empty() {
            "none".to_string()
        } else {
            r.audiences.join(", ")
        },
        r.things,
        r.clusters,
        r.calls,
        r.failed,
        r.left,
        r.ended,
        r.linked,
        r.aliased,
        r.refused,
        r.released,
        r.recorded,
        r.commits,
        ex.calls()
    );
    Ok(())
}

/// `user` or `user:group1,group2`.
fn principal(user: &str) -> Principal {
    let (user, groups) = user.split_once(':').unwrap_or((user, ""));
    let groups: Vec<&str> = groups.split(',').filter(|g| !g.is_empty()).collect();
    Principal::new(user, &groups)
}

/// Whether `cmd` builds a memory: what it asks the models is kept in the model-call cache unless
/// `MEMSTORE_MODEL_CACHE` says otherwise, so building the same memory again (a fresh store of the same inputs, a
/// rebuild) pays for no call twice. A server serving people keeps none unless told.
fn builds(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Backfill { .. }
            | Cmd::Compile { .. }
            | Cmd::Extract { .. }
            | Cmd::Dream { .. }
            | Cmd::Advice { .. }
            | Cmd::Aggregate { .. }
            | Cmd::Load { .. }
            | Cmd::Serve { local: Some(_), .. }
    )
}

async fn run(cmd: Cmd, direct: bool) -> Result<()> {
    if builds(&cmd) {
        memstore::extract::cache::default_to(memstore::extract::cache::build_location());
    }
    // Commands that need no catalog.
    match &cmd {
        // Brings its own: everything under one directory.
        Cmd::Serve {
            scope,
            port,
            groups,
            local: Some(dir),
            ..
        } => {
            let groups = groups.as_deref().map_or(vec![], memstore::split_list);
            return serve_local(dir, *port, Scope::parse(scope)?, groups).await;
        }
        Cmd::Index(IndexArgs {
            cmd: None,
            scope: Some(scope),
            wait,
            branch,
            timeout,
            local: Some(dir),
        }) => {
            let store = local_store(dir).await?;
            if !*wait {
                return index_scope(&store, &Scope::parse(scope)?).await;
            }
            let s = memstore::search::manage::wait(
                &store,
                &Scope::parse(scope)?,
                branch.as_deref(),
                std::time::Duration::from_secs(*timeout),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&s)?);
            return Ok(());
        }
        Cmd::Check(
            c @ CheckCmd::Collisions {
                local: Some(dir), ..
            },
        ) => {
            let catalog = memstore::platform::local_dir(dir).await?;
            let store = on_local(dir, catalog.clone());
            println!("{}", check(c.clone(), catalog, &store).await?);
            return Ok(());
        }
        Cmd::Extract {
            scope,
            limit,
            dry_run,
            branch,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return extract(&store, scope, branch, *limit, *dry_run).await;
        }
        Cmd::Dream {
            scope,
            dry_run,
            calls,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return dream(&store, scope, *dry_run, *calls).await;
        }
        Cmd::Load {
            dir: corpus_dir,
            episodes_per_commit,
            only,
            workspace,
            branch,
            fresh,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            if let Some(ws) = &workspace {
                memstore::workspace::get(&store, ws).await?;
            }
            println!(
                "{}",
                corpus::load_into(
                    &store,
                    corpus_dir,
                    *episodes_per_commit,
                    only.as_deref(),
                    workspace.as_deref(),
                    branch,
                    *fresh
                )
                .await?
            );
            return Ok(());
        }
        Cmd::Schema(SchemaCmd::Get {
            scope,
            branch,
            local: Some(dir),
        }) => {
            let store = local_store(dir).await?;
            return schema_get(&store, scope, branch).await;
        }
        Cmd::Schema(SchemaCmd::Set {
            scope,
            schema,
            branch,
            breaking,
            dry_run,
            local: Some(dir),
        }) => {
            let store = local_store(dir).await?;
            return schema_set(&store, scope, schema, branch, *breaking, *dry_run).await;
        }
        Cmd::Backfill {
            scope,
            dry_run,
            max_episodes,
            max_usd,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return backfill(&store, scope, *dry_run, *max_episodes, *max_usd).await;
        }
        Cmd::Compile {
            scope,
            sources,
            branch,
            limit,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return compile(&store, scope, sources.as_deref(), branch, *limit).await;
        }
        Cmd::Advice {
            scope,
            acl,
            min_wrong,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return advice(&store, scope, acl, *min_wrong).await;
        }
        Cmd::Runs(
            r @ RunsCmd::Load {
                local: Some(dir), ..
            },
        ) => {
            let store = local_store(dir).await?;
            return runs(&store, r.clone()).await;
        }
        Cmd::TrainLog(
            c @ (TrainLogCmd::Sweep {
                local: Some(dir), ..
            }
            | TrainLogCmd::Export {
                local: Some(dir), ..
            }),
        ) => {
            let store = local_store(dir).await?;
            return train_log(&store, c.clone()).await;
        }
        Cmd::QaLog {
            scope,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return qa_log(&store, scope).await;
        }
        Cmd::Maintain {
            scope,
            keep,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return maintain(&store, scope, *keep).await;
        }
        Cmd::Brief {
            scope,
            user,
            subject,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            let session = store.pin(&Scope::parse(scope)?, memstore::MAIN).await?;
            let v = memstore::brief::brief(&session, &store, &principal(user), subject).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        Cmd::Aggregate {
            scope,
            now,
            subject,
            dry_run,
            show,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return aggregate(&store, scope, *now, subject.as_deref(), *dry_run, *show).await;
        }
        Cmd::Hooks {
            action,
            harness,
            url,
            dry_run,
        } => {
            let h = memstore::hook::Harness::parse(harness)?;
            let remove = match action.as_str() {
                "install" => false,
                "uninstall" => true,
                other => anyhow::bail!("memstore hooks install|uninstall, not {other}"),
            };
            let changes = memstore::hook_install::apply(h, url.as_deref(), remove, *dry_run)?;
            let lead = if *dry_run { "would change" } else { "changed" };
            if changes.is_empty() {
                println!("{}: nothing to change", h.name());
            }
            for c in &changes {
                println!("{lead} {}: {}", c.path.display(), c.what);
            }
            if !remove
                && memstore::hook_install::writes_token(h)
                && std::env::var("MEMSTORE_TOKEN").is_ok()
            {
                println!(
                    "note: {} can't read MEMSTORE_TOKEN from the environment, so its MCP entry holds the token",
                    h.name()
                );
            }
            if !remove && !changes.is_empty() {
                println!(
                    "restart {} to load them; `memstore hook status` checks the server",
                    h.name()
                );
            }
            return Ok(());
        }
        Cmd::Hook {
            event,
            args,
            harness,
            session,
        } => {
            let r = match memstore::hook::Harness::parse(harness) {
                Ok(_) if event == "outcome" => {
                    memstore::hook::outcome(session.as_deref(), args).await
                }
                Ok(h) => memstore::hook::run_in(event, h).await,
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                eprintln!("memstore hook {event}: {e:#}");
            }
            return Ok(());
        }
        Cmd::Mcp => return memstore::hook::bridge().await,
        Cmd::Doctor => return memstore::hook::doctor().await,
        Cmd::SupportBundle {
            engine,
            admin_key_file,
            out,
            project,
            region,
            service,
            logs_file,
            since_minutes,
        } => {
            let logs = match (project, logs_file) {
                (Some(p), _) => memstore::support_bundle::Logs::CloudRun {
                    project: p.clone(),
                    region: region.clone(),
                    service: service.clone(),
                },
                (None, Some(f)) => memstore::support_bundle::Logs::File(f.clone()),
                (None, None) => memstore::support_bundle::Logs::None,
            };
            let dir = memstore::support_bundle::make(
                engine,
                admin_key_file.as_deref(),
                out.as_deref(),
                logs,
                *since_minutes,
            )
            .await?;
            println!("{}", dir.display());
            return Ok(());
        }
        Cmd::Preflight(PreflightCmd::Gcp {
            project,
            region,
            variant,
            runtime,
            name,
            bucket,
            vendor_customer_id,
            control_plane,
            image_registry,
            installed,
            cosign_key,
            credential,
            signature_repository,
            image,
            json,
            explain,
        }) => {
            use memstore::preflight;
            if *explain {
                print!("{}", preflight::explain());
                return Ok(());
            }
            let t = preflight::Target {
                project: project.clone(),
                region: region.clone(),
                variant: *variant,
                runtime: *runtime,
                name: name.clone(),
                bucket: bucket.clone(),
                vendor_customer_id: vendor_customer_id.clone(),
                control_plane: control_plane.clone(),
                image_registry: image_registry.clone(),
                signature_repository: signature_repository.clone(),
                image: image.clone(),
            };
            let report = if *installed {
                preflight::run_installed(
                    &t,
                    &mut memstore::provision::Shell,
                    *json,
                    cosign_key.as_deref(),
                    credential.as_deref(),
                )
                .await
            } else {
                preflight::run(&t, &mut memstore::provision::Shell, *json).await
            };
            let report = match report {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("memstore preflight couldn't run: {e:#}");
                    std::process::exit(3);
                }
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                report.print(VERSION);
            }
            std::process::exit(report.exit_code());
        }
        #[cfg(feature = "control-plane")]
        Cmd::Cloud(c) => return memstore::cloud::run(c).await,
        Cmd::People(p) if !memstore::people::cli::needs_catalog(p, direct)? => {
            return memstore::people::cli::run(p, None).await;
        }
        Cmd::VerifyCorpus { dir } => {
            println!("{}", corpus::verify(dir)?);
            return Ok(());
        }
        Cmd::Gen {
            seed,
            episodes,
            dir,
        } => {
            let dir = (dir != "-").then(|| std::path::Path::new(dir));
            let m = corpus::write(*seed, *episodes, dir)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            return Ok(());
        }
        // Fetches the model into MEMSTORE_MODEL_DIR when building an image.
        Cmd::Embed { text } => {
            let v = memstore::search::embed_text(&text.join(" ")).await?;
            println!(
                "{}: {} dims, first {:?}; embedding cache: {}",
                memstore::search::embed::provider().describe(),
                v.len(),
                &v[..3.min(v.len())],
                memstore::search::embed_cache::describe()
            );
            return Ok(());
        }
        Cmd::Schema(SchemaCmd::List) => {
            for name in memstore::query::schema::BUILTIN {
                println!(
                    "{}\n",
                    memstore::query::schema::Schema::load(name)?.summary()
                );
            }
            return Ok(());
        }
        #[cfg(feature = "control-plane")]
        Cmd::Init(a) => return memstore::onboard::init(a).await,
        Cmd::Deployment(d) => {
            use memstore::deployment::{self, Credential};
            let path = |c: &Option<PathBuf>| c.clone().unwrap_or_else(Credential::default_path);
            match d {
                DeploymentCmd::Enroll {
                    control_plane,
                    token,
                    credential,
                } => {
                    let p = path(credential);
                    let c = deployment::enroll(control_plane, token, &p).await?;
                    println!(
                        "enrolled as {} (credential in {})",
                        String::from(c.deployment),
                        p.display()
                    );
                }
                DeploymentCmd::Rotate { credential } => {
                    deployment::rotate(&path(credential)).await?;
                    println!("rotated");
                }
                DeploymentCmd::Send { file, credential } => {
                    let text = if file == "-" {
                        std::io::read_to_string(std::io::stdin())?
                    } else {
                        std::fs::read_to_string(file)?
                    };
                    memstore::telemetry::check(&text)?;
                    let batch: Vec<serde_json::Value> = text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(serde_json::from_str)
                        .collect::<Result<_, _>>()?;
                    let c = Credential::load(&path(credential))?;
                    let (status, body) = c
                        .call("POST", "/v1/telemetry", &serde_json::Value::Array(batch))
                        .await?;
                    anyhow::ensure!(status == 200, "refused ({status}): {}", body["error"]);
                    println!("accepted {}", body["accepted"]);
                }
                DeploymentCmd::Whoami { credential } => {
                    let c = Credential::load(&path(credential))?;
                    let (status, body) =
                        c.call("GET", "/v1/whoami", &serde_json::json!({})).await?;
                    anyhow::ensure!(status == 200, "refused ({status}): {}", body["error"]);
                    println!("{}", body["deployment"].as_str().unwrap_or_default());
                }
            }
            return Ok(());
        }
        Cmd::Agent(a) => {
            use memstore::agent::{self, Agent, Config, Target};
            let credential = a
                .credential
                .clone()
                .unwrap_or_else(memstore::deployment::Credential::default_path);
            let target: Box<dyn Target> = match a.target.as_str() {
                "kubernetes" => Box::new(
                    match &a.kube_api {
                        Some(api) => agent::Kubernetes::at(
                            api,
                            None,
                            None,
                            a.namespace.clone().unwrap_or_else(|| "default".into()),
                            a.deployment.clone(),
                            a.container.clone(),
                        )?,
                        None => agent::Kubernetes::in_cluster(
                            a.namespace.clone(),
                            a.deployment.clone(),
                            a.container.clone(),
                        )?,
                    }
                    .with_also(a.also_deployments.clone())?,
                ),
                "cloud-run" => Box::new(agent::CloudRun),
                "snowflake" => Box::new(agent::Snowflake),
                t => anyhow::bail!("--target is kubernetes, cloud-run or snowflake, not {t}"),
            };
            let namespace = match &a.namespace {
                Some(n) => n.clone(),
                None => std::fs::read_to_string(
                    "/var/run/secrets/kubernetes.io/serviceaccount/namespace",
                )
                .map(|n| n.trim().to_string())
                .unwrap_or_else(|_| "default".into()),
            };
            let health_token = a
                .health_token_file
                .as_ref()
                .map(|f| std::fs::read_to_string(f).map(|t| t.trim().to_string()))
                .transpose()?;
            let admin_key = a
                .admin_key_file
                .as_ref()
                .map(|f| std::fs::read_to_string(f).map(|t| t.trim().to_string()))
                .transpose()?;
            let cfg = Config {
                state: a.state.clone().unwrap_or_else(|| {
                    Config::default_state(&credential, a.telemetry_mirror.clone())
                }),
                credential,
                telemetry_mirror: a.telemetry_mirror.clone(),
                control_plane_key: a.control_plane_key.clone(),
                trust_on_first_use: a.trust_control_plane_key_on_first_use,
                image_repository: a.image_repository.clone(),
                drift_mode: a.drift_mode,
                embed_providers: a
                    .embed_providers
                    .iter()
                    .map(|p| p.trim().to_string())
                    .collect(),
                health_url: a.health_url.clone().unwrap_or_else(|| {
                    format!("http://{}.{namespace}.svc:8080/healthz", a.deployment)
                }),
                health_token,
                poll: std::time::Duration::from_secs(a.poll_seconds.max(1)),
                rollout_timeout: std::time::Duration::from_secs(a.rollout_timeout_seconds),
                health_timeout: std::time::Duration::from_secs(a.health_timeout_seconds),
                check_every: std::time::Duration::from_secs(5),
                cosign_key: a
                    .cosign_key
                    .as_deref()
                    .map(memstore::cosign::PublicKey::from_pem_file)
                    .transpose()?,
                allow_unsigned_images: a.allow_unsigned_images,
                registry_auth: a
                    .registry_auth
                    .as_deref()
                    .map(memstore::cosign::RegistryAuth::parse)
                    .transpose()?,
                registry_api: a.registry_url.clone(),
                metadata_host: a.metadata_host.clone(),
                signature_repository: a.signature_repository.clone(),
                engine_url: a.engine_url.clone(),
                admin_key,
                support_poll: std::time::Duration::from_secs(a.support_poll_seconds.max(1)),
            };
            let mut agent = Agent::start(cfg, target).await?;
            if a.once {
                agent.tick().await?;
            } else {
                agent.run().await?;
            }
            return Ok(());
        }
        Cmd::Telemetry(TelemetryCmd::Check { file }) => {
            let text = if file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(file)?
            };
            let n = memstore::telemetry::check(&text)?;
            println!(
                "{n} payloads hold to contract {}",
                memstore::telemetry::CONTRACT
            );
            return Ok(());
        }
        Cmd::Telemetry(TelemetryCmd::Schema) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&memstore::telemetry::schema())?
            );
            return Ok(());
        }
        Cmd::Schema(SchemaCmd::Check { schema }) => {
            println!(
                "{}",
                memstore::query::schema::Schema::load(schema)?.summary()
            );
            return Ok(());
        }
        Cmd::RetractFacts {
            scope,
            fact,
            extractor,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return retract_facts(&store, scope, fact.as_deref(), extractor.as_deref()).await;
        }
        Cmd::CompactLedger {
            dry_run,
            aggregates,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return compact_ledger(&store, *dry_run, aggregates.as_deref()).await;
        }
        Cmd::MergeSubjects {
            scope,
            from,
            into,
            plan,
            local: Some(dir),
        } => {
            let store = local_store(dir).await?;
            return merge_subjects(
                &store,
                scope,
                from.as_deref(),
                into.as_deref(),
                plan.as_deref(),
            )
            .await;
        }
        Cmd::EpisodeType(e) => return episode_type(e),
        Cmd::Dataset(d) => return dataset(d).await,
        _ => {}
    }
    let catalog = platform::catalog_from_env().await?;
    let store = Store::new(catalog.clone());
    match cmd {
        Cmd::Smoke { scope } => println!("{}", smoke(&store, &Scope::parse(&scope)?).await?),
        Cmd::Put {
            scope,
            acl,
            entity,
            predicate,
            value,
        } => {
            // A claim is recorded as its own episode, so the fact has an access set (D-6, D-8).
            let scope = Scope::parse(&scope)?;
            store.ensure_scope(&scope).await?;
            let acl: Vec<&str> = acl.split(',').collect();
            let claim = format!("cli claim: {entity} {predicate} {value}");
            let ep = Episode::new("note:cli:claim", claim.as_bytes(), &acl);
            memstore::episode_types::check_all(std::slice::from_ref(&ep))?;
            let fact = Fact::new(&entity, &predicate, &value).from_episodes(&[&ep]);
            let id = store.commit(&scope, "cli", &[ep], &[fact]).await?;
            println!("committed {id}");
        }
        Cmd::Ingest {
            scope,
            source,
            acl,
            body,
        } => {
            let scope = Scope::parse(&scope)?;
            store.ensure_scope(&scope).await?;
            let acl: Vec<&str> = acl.split(',').collect();
            let ep = Episode::new(&source, body.as_bytes(), &acl);
            memstore::episode_types::check_all(std::slice::from_ref(&ep))?;
            store
                .commit(&scope, "cli", std::slice::from_ref(&ep), &[])
                .await?;
            println!("episode {}", ep.episode_id);
        }
        Cmd::Close {
            scope,
            fact_id,
            when,
            acl,
            reason,
        } => {
            let scope = Scope::parse(&scope)?;
            let end = memstore::query::schema::parse_instant(&when)?;
            let acl: Vec<&str> = acl.split(',').collect();
            let body = format!("cli closure of {fact_id} at {when}: {}", reason.join(" "));
            let ep = Episode::new("note:cli:close", body.as_bytes(), &acl);
            memstore::episode_types::check_all(std::slice::from_ref(&ep))?;
            let id = store
                .close_facts(
                    &scope,
                    memstore::MAIN,
                    "cli",
                    &[(fact_id.as_str(), end, &ep)],
                )
                .await?;
            println!("committed {id} (episode {})", ep.episode_id);
        }
        Cmd::Get {
            scope,
            user,
            groups,
        } => {
            let groups: Vec<&str> = groups.iter().flat_map(|g| g.split(',')).collect();
            let who = Principal::new(&user, &groups);
            for f in store.read(&Scope::parse(&scope)?, &who, None).await? {
                println!("{}\t{}\t{}\t{}", f.entity, f.predicate, f.value, f.fact_id);
            }
        }
        Cmd::Bench {
            scope,
            reads,
            gap_ms,
        } => bench(&store, &Scope::parse(&scope)?, reads, gap_ms).await?,
        Cmd::Maintain { scope, keep, .. } => maintain(&store, &scope, keep).await?,
        Cmd::CompactLedger {
            dry_run,
            aggregates,
            ..
        } => compact_ledger(&store, dry_run, aggregates.as_deref()).await?,
        Cmd::Schema(SchemaCmd::Get { scope, branch, .. }) => {
            schema_get(&store, &scope, &branch).await?
        }
        Cmd::Schema(SchemaCmd::Set {
            scope,
            schema,
            branch,
            breaking,
            dry_run,
            ..
        }) => schema_set(&store, &scope, &schema, &branch, breaking, dry_run).await?,
        Cmd::Brief {
            scope,
            user,
            subject,
            ..
        } => {
            let session = store.pin(&Scope::parse(&scope)?, memstore::MAIN).await?;
            let v = memstore::brief::brief(&session, &store, &principal(&user), &subject).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::QaLog { scope, .. } => qa_log(&store, &scope).await?,
        Cmd::Runs(r) => runs(&store, r).await?,
        Cmd::TrainLog(c) => train_log(&store, c).await?,
        Cmd::Compile {
            scope,
            sources,
            branch,
            limit,
            ..
        } => compile(&store, &scope, sources.as_deref(), &branch, limit).await?,
        Cmd::Backfill {
            scope,
            dry_run,
            max_episodes,
            max_usd,
            ..
        } => backfill(&store, &scope, dry_run, max_episodes, max_usd).await?,
        Cmd::Advice {
            scope,
            acl,
            min_wrong,
            ..
        } => advice(&store, &scope, &acl, min_wrong).await?,
        Cmd::RetractFacts {
            scope,
            fact,
            extractor,
            ..
        } => retract_facts(&store, &scope, fact.as_deref(), extractor.as_deref()).await?,
        Cmd::MergeSubjects {
            scope,
            from,
            into,
            plan,
            ..
        } => {
            merge_subjects(
                &store,
                &scope,
                from.as_deref(),
                into.as_deref(),
                plan.as_deref(),
            )
            .await?
        }
        Cmd::Query { scope, user, query } => {
            let text = match query.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)?,
                None => query,
            };
            let ir: serde_json::Value = serde_json::from_str(&text)?;
            let session = store.pin(&Scope::parse(&scope)?, memstore::MAIN).await?;
            let t = std::time::Instant::now();
            let answer =
                memstore::query::run_json(&store, &session, &principal(&user), &ir).await?;
            println!("{}", serde_json::to_string_pretty(&answer)?);
            eprintln!("{} ms", t.elapsed().as_millis());
        }
        Cmd::Index(IndexArgs { cmd: Some(c), .. }) => index_cmd(&store, catalog, c).await?,
        Cmd::Index(IndexArgs {
            cmd: None,
            scope: Some(scope),
            wait: true,
            branch,
            timeout,
            ..
        }) => {
            let s = memstore::search::manage::wait(
                &store,
                &Scope::parse(&scope)?,
                branch.as_deref(),
                std::time::Duration::from_secs(timeout),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        Cmd::Index(IndexArgs {
            cmd: None,
            scope: None,
            ..
        }) => anyhow::bail!("memstore index <scope>, or index status|rebuild|verify|list|drop"),
        Cmd::Index(IndexArgs {
            cmd: None,
            scope: Some(scope),
            ..
        }) => {
            index_scope(&store, &Scope::parse(&scope)?).await?;
        }
        Cmd::Serve {
            scope,
            port,
            groups,
            local,
            workspaces,
        } => {
            let groups = groups.map_or(vec![], |g| memstore::split_list(&g));
            anyhow::ensure!(
                local.is_none(),
                "--local is served before a catalog is opened"
            );
            memstore::server::serve_with(
                port,
                Scope::parse(&scope)?,
                groups,
                memstore::server::Options {
                    workspaces,
                    ..Default::default()
                },
            )
            .await?;
        }
        Cmd::SearchBatch { input, output } => search_batch(&store, &input, &output).await?,
        Cmd::Search { scope, user, text } => {
            let t = std::time::Instant::now();
            let hits = memstore::search::search(
                &store,
                &Scope::parse(&scope)?,
                memstore::MAIN,
                &principal(&user),
                &text.join(" "),
                10,
                Some(i64::MAX),
            )
            .await?;
            for h in &hits {
                println!(
                    "{:>2} {:>7.3} {:>6.4} {}{} {} {}",
                    h.terms_matched,
                    h.bm25,
                    h.score,
                    if h.from_tail { "(tail) " } else { "" },
                    h.fact.entity,
                    h.fact.predicate,
                    h.fact.value
                );
            }
            println!("{} hits in {} ms", hits.len(), t.elapsed().as_millis());
        }
        Cmd::Extract {
            scope,
            limit,
            dry_run,
            branch,
            ..
        } => extract(&store, &scope, &branch, limit, dry_run).await?,
        Cmd::Dream {
            scope,
            dry_run,
            calls,
            ..
        } => dream(&store, &scope, dry_run, calls).await?,
        Cmd::Aggregate {
            scope,
            now,
            subject,
            dry_run,
            show,
            ..
        } => aggregate(&store, &scope, now, subject.as_deref(), dry_run, show).await?,
        Cmd::BackfillQuotes { scope, redo } => {
            println!(
                "{}",
                store
                    .backfill_quotes(&Scope::parse(&scope)?, "backfill", redo)
                    .await?
            );
        }
        Cmd::Load {
            dir,
            episodes_per_commit,
            only,
            workspace,
            branch,
            fresh,
            local: _,
        } => {
            if let Some(ws) = &workspace {
                memstore::workspace::get(&store, ws).await?;
            }
            println!(
                "{}",
                corpus::load_into(
                    &store,
                    &dir,
                    episodes_per_commit,
                    only.as_deref(),
                    workspace.as_deref(),
                    &branch,
                    fresh
                )
                .await?
            );
        }
        Cmd::Workspace(w) => workspace(&store, w).await?,
        Cmd::Eval { dir, scope, warm } => {
            let scope = scope.as_deref().filter(|s| *s != "all");
            if warm {
                corpus::eval(&store, &dir, scope).await?;
            }
            let r = corpus::eval(&store, &dir, scope).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
            let wrong: usize = r.by_category.values().map(|(ok, n)| n - ok).sum();
            anyhow::ensure!(wrong == 0, "{wrong} gold questions answered wrong");
        }
        Cmd::Check(c) => println!("{}", check(c, catalog, &store).await?),
        Cmd::People(p) => memstore::people::cli::run(&p, Some(&store)).await?,
        Cmd::VerifyCorpus { .. }
        | Cmd::Hook { .. }
        | Cmd::Mcp
        | Cmd::Doctor
        | Cmd::Preflight(_)
        | Cmd::SupportBundle { .. }
        | Cmd::Hooks { .. }
        | Cmd::Gen { .. }
        | Cmd::Embed { .. }
        | Cmd::Dataset(_)
        | Cmd::Schema(SchemaCmd::List)
        | Cmd::Schema(SchemaCmd::Check { .. })
        | Cmd::EpisodeType(_)
        | Cmd::Telemetry(_)
        | Cmd::Deployment(_)
        | Cmd::Agent(_) => {
            unreachable!("handled before the catalog")
        }
        #[cfg(feature = "control-plane")]
        Cmd::Cloud(_) | Cmd::Init(_) => unreachable!("handled before the catalog"),
    }
    Ok(())
}

/// `episode-type`: the episode types, their catalogs, and how a body splits into the units facts cite.
fn episode_type(cmd: &EpisodeTypeCmd) -> Result<()> {
    use memstore::episode_types as et;
    match cmd {
        EpisodeTypeCmd::List => {
            for t in et::all() {
                let c = et::catalog(*t)?;
                let preds: usize = c.types.values().map(|d| d.predicates.len()).sum();
                let prefixes = t.source_prefixes().join(" ");
                println!(
                    "{:13} {:11} {:3} predicates  model: {:3}  sources: {prefixes}",
                    t.name(),
                    format!("{:?}", t.format()).to_lowercase(),
                    preds,
                    if t.uses_model() { "yes" } else { "no" },
                );
            }
        }
        EpisodeTypeCmd::Show { name } => {
            let t = et::by_name(name)?;
            println!("{}\n", et::catalog(t)?.summary());
            println!(
                "items (named <type>:<episode>#<n> by the engine): {}",
                t.items().join(", ")
            );
            println!(
                "written by the engine, not the model: {}",
                t.engine_predicates().join(", ")
            );
            println!("instructions: {}", t.instructions());
        }
        EpisodeTypeCmd::Units {
            name,
            file,
            source,
            quote,
        } => {
            let body = std::fs::read(file).map_err(|e| anyhow::anyhow!("read {file}: {e}"))?;
            let mut ep = memstore::Episode::new(source, &body, &["*"]);
            ep.source = source.clone();
            let t = if name == "auto" {
                et::of(&ep).ok_or_else(|| {
                    anyhow::anyhow!("source {source:?} is of no episode type: {}", et::known())
                })?
            } else {
                et::by_name(name)?
            };
            let units = t.units(&ep)?;
            println!("{} ({:?}): {} units", t.name(), t.format(), units.len());
            match quote {
                None => {
                    for u in units.iter().take(20) {
                        println!(
                            "  {:>4}  {:28}  {}",
                            u.index,
                            u.cite.describe(),
                            u.text.chars().take(90).collect::<String>()
                        );
                    }
                    for f in t.engine_facts(&ep, &units)? {
                        println!(
                            "  engine fact: {} {} = {}  ({})",
                            f.subject,
                            f.predicate,
                            f.value,
                            f.cite.describe()
                        );
                    }
                }
                Some(q) => match et::locate(&units, q, None) {
                    Some(u) => println!(
                        "cites unit {}: {}  {}",
                        u.index,
                        u.cite.describe(),
                        serde_json::to_string(&u.cite)?
                    ),
                    None => anyhow::bail!("the quote is in no unit"),
                },
            }
        }
    }
    Ok(())
}

/// `memstore backfill`: read the history the schema's `extract` section names with the schema as it stands, within
/// the limits given.
async fn backfill(
    store: &Store,
    scope: &str,
    dry_run: bool,
    max_episodes: usize,
    max_usd: Option<f64>,
) -> Result<()> {
    let ex = memstore::extract::Extractor::from_env()?.ok_or_else(|| anyhow::anyhow!(NO_MODEL))?;
    let price = extract_price()?;
    let opts = memstore::extract::goals::Backfill {
        dry_run,
        max_episodes,
        max_usd,
        price,
    };
    let r = memstore::extract::goals::backfill(store, &Scope::parse(scope)?, &ex, &opts).await?;
    println!("{}", serde_json::to_string_pretty(&r)?);
    Ok(())
}

/// Bring a scope's facts and passage indexes up to date (`memstore index <scope>`).
async fn index_scope(store: &Store, scope: &Scope) -> Result<()> {
    // Texts embedded by the model and reused from the embedding cache, during what follows.
    let since = |(h0, m0): (u64, u64)| {
        let (h, m) = memstore::search::embed_cache::counts();
        (m - m0, h - h0)
    };
    let (t, c) = (
        std::time::Instant::now(),
        memstore::search::embed_cache::counts(),
    );
    let (rows, files) = memstore::search::update(store, scope).await?;
    let (embedded, reused) = since(c);
    println!(
        "indexed {rows} rows from {files} new data files ({embedded} embedded, {reused} from the embedding cache) in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let (t, c) = (
        std::time::Instant::now(),
        memstore::search::embed_cache::counts(),
    );
    let (passages, files, _) = memstore::search::passages::update(store, scope).await?;
    let (embedded, reused) = since(c);
    println!(
        "indexed {passages} passages ({embedded} embedded, {reused} from the embedding cache, the rest kept their vectors) from {files} new episodes files in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn index_cmd(
    store: &Store,
    catalog: std::sync::Arc<dyn iceberg::Catalog>,
    c: IndexCmd,
) -> Result<()> {
    use memstore::search::manage;
    let elsewhere = |base: &str| {
        let (bytes, overlap) = store.chunking();
        Store::new(catalog.clone())
            .with_index_base(base)
            .with_chunking(bytes, overlap)
    };
    match c {
        IndexCmd::Status { scope } => {
            let s = manage::status(store, &Scope::parse(&scope)?).await?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        IndexCmd::Rebuild { scope, to } => {
            println!(
                "{}",
                manage::rebuild(&elsewhere(&to), &Scope::parse(&scope)?).await?
            );
        }
        IndexCmd::Verify { scope, against } => {
            let temp =
                std::env::temp_dir().join(format!("memstore-verify-{}", uuid::Uuid::new_v4()));
            let base = against.unwrap_or_else(|| temp.display().to_string());
            let r = manage::verify(store, &elsewhere(&base), &Scope::parse(&scope)?).await;
            let _ = std::fs::remove_dir_all(&temp);
            println!("{}", r?);
        }
        IndexCmd::List { scope } => {
            for i in manage::list(store, &Scope::parse(&scope)?).await? {
                println!("{i}");
            }
        }
        IndexCmd::Drop { scope, dir } => {
            manage::drop_dir(store, &Scope::parse(&scope)?, &dir).await?;
            println!("dropped {dir} of {scope}");
        }
    }
    Ok(())
}

async fn workspace(store: &Store, w: WorkspaceCmd) -> Result<()> {
    use memstore::workspace as ws;
    let owner = memstore::people::cli::owner();
    let pretty = |v: &serde_json::Value| serde_json::to_string_pretty(v).unwrap_or_default();
    match w {
        WorkspaceCmd::Create {
            ws: name,
            schema,
            ttl,
            max_episodes,
            max_facts,
            max_index_bytes,
        } => {
            let loaded = match &schema {
                Some(s) => Some(memstore::query::schema::Schema::load(s)?),
                None => None,
            };
            let expires_at = match ttl {
                Some(t) => Some(
                    chrono::Utc::now().timestamp_micros() + ws::parse_ttl(&t)?.as_micros() as i64,
                ),
                None => None,
            };
            let spec = ws::Spec {
                schema,
                expires_at,
                limits: ws::Limits {
                    max_episodes,
                    max_facts,
                    max_index_bytes,
                },
            };
            println!(
                "{}",
                pretty(&ws::create(store, &owner, &name, spec, loaded.as_ref()).await?)
            );
        }
        WorkspaceCmd::List => {
            for w in ws::list(store).await? {
                println!("{w}");
            }
        }
        WorkspaceCmd::Show { ws: name } => println!("{}", pretty(&ws::show(store, &name).await?)),
        WorkspaceCmd::Branch { ws: name, branch } => {
            ws::branch(store, &owner.user, &name, &branch).await?;
            println!("{name}: branch {branch} from main");
        }
        WorkspaceCmd::Snapshot {
            ws: name,
            label,
            branch,
        } => {
            let s = ws::snapshot(store, &owner.user, &name, &branch, label.as_deref()).await?;
            println!("{}", s.id);
            for gap in ws::unindexed(store, &s).await? {
                eprintln!("note: {gap}");
            }
        }
        WorkspaceCmd::Delete { ws: name, confirm } => {
            println!(
                "{}",
                ws::delete(store, &owner, &name, confirm.as_deref()).await?
            );
        }
        WorkspaceCmd::Gc => {
            let gone = ws::gc(store, &owner).await?;
            if gone.is_empty() {
                println!("no workspace is past its time to live");
            }
            for g in gone {
                println!("{g}");
            }
        }
    }
    Ok(())
}

async fn dataset(d: &DatasetCmd) -> Result<()> {
    match d {
        DatasetCmd::List => {
            for (name, latest) in memstore::harness::dataset::list().await? {
                println!("{name}\t{latest}");
            }
        }
        DatasetCmd::Get { spec, dir } => {
            let name = spec.split('@').next().unwrap_or(spec);
            let dir = dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("corpora/{name}")));
            let (fetched, kept) = memstore::harness::dataset::get(spec, &dir).await?;
            println!(
                "{spec} -> {}: {fetched} fetched, {kept} already there",
                dir.display()
            );
        }
        DatasetCmd::Put {
            name,
            dir,
            version,
            description,
        } => {
            let version = version
                .clone()
                .unwrap_or_else(|| chrono::Utc::now().format("%Y%m%d-%H%M").to_string());
            let n = memstore::harness::dataset::put(name, dir, &version, description).await?;
            println!("{name}@{version}: {n} files");
        }
    }
    Ok(())
}

async fn check(
    c: CheckCmd,
    catalog: std::sync::Arc<dyn iceberg::Catalog>,
    store: &Store,
) -> Result<String> {
    use memstore::harness::checks;
    Ok(match c {
        CheckCmd::Concurrent {
            scope,
            writers,
            commits,
            shared,
        } => {
            checks::concurrent_commits(catalog, &Scope::parse(&scope)?, writers, commits, shared)
                .await?
        }
        CheckCmd::Collisions { scope, .. } => {
            let shared = store.shared_episodes(&Scope::parse(&scope)?).await?;
            let mut out = format!(
                "{} episode(s) with rows from more than one source\n",
                shared.len()
            );
            for (id, rows) in shared {
                out.push_str(&format!("{id}\n"));
                for (source, acl, at) in rows {
                    let when = chrono::DateTime::from_timestamp_micros(at)
                        .map_or(at.to_string(), |t| t.to_rfc3339());
                    out.push_str(&format!("  {when}\t{source}\t{}\n", acl.join(",")));
                }
            }
            out.trim_end().to_string()
        }
        CheckCmd::Pinning { scope, writes } => {
            checks::snapshot_pinning(store, &Scope::parse(&scope)?, writes).await?
        }
        CheckCmd::Crash {
            scope,
            crashes,
            seed,
        } => checks::crash_atomicity(store, &Scope::parse(&scope)?, crashes, seed).await?,
        CheckCmd::Model {
            sequences,
            ops,
            seed,
        } => checks::branch_model(store, sequences, ops, seed).await?,
        CheckCmd::Skew {
            scope,
            writers,
            commits,
            skew_secs,
        } => {
            let skew = std::time::Duration::from_secs(skew_secs);
            checks::clock_skew(catalog, &Scope::parse(&scope)?, writers, commits, skew).await?
        }
        CheckCmd::Freshness { scope, samples } => {
            let (p50, p95) = checks::freshness(catalog, &Scope::parse(&scope)?, samples).await?;
            anyhow::ensure!(
                p95 < 300_000.0,
                "freshness p95 {p95:.0} ms over NFR-3 (5 min)"
            );
            format!(
                "ok: write visible to another warm reader p50 {p50:.0} ms, p95 {p95:.0} ms (NFR-3: 5 min)"
            )
        }
        CheckCmd::Throughput {
            scope,
            writers,
            commits,
            facts,
            shared,
        } => {
            checks::throughput(
                catalog,
                &Scope::parse(&scope)?,
                writers,
                commits,
                facts,
                shared,
            )
            .await?
        }
        CheckCmd::IndexEquivalence {
            scope,
            batches,
            per_batch,
            seed,
        } => {
            checks::index_equivalence(catalog, &Scope::parse(&scope)?, batches, per_batch, seed)
                .await?
        }
        CheckCmd::SourceAcl { dir, sample, seed } => {
            let only = corpus::scope_filter(&dir, Some("questions"))?.unwrap_or_default();
            checks::source_acl(store, &dir, &only, sample, seed).await?
        }
    })
}

async fn bench(store: &Store, scope: &Scope, n: usize, gap: u64) -> Result<()> {
    let who = Principal::new("bench", &[]);
    let mut ms = vec![];
    let mut facts = 0;
    for _ in 0..n {
        let t = std::time::Instant::now();
        facts = store.read(scope, &who, None).await?.len();
        ms.push(t.elapsed().as_millis());
        tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
    }
    let first = ms[0];
    ms.sort();
    println!(
        "{n} reads of {} ({facts} visible facts): first {first} ms, p50 {} ms, p95 {} ms",
        scope.namespace(),
        ms[ms.len() / 2],
        ms[(ms.len() * 95).div_ceil(100) - 1]
    );
    // Freshness: a second store writes; this (warm) store polls until it sees it.
    let writer = Store::new(platform::catalog_from_env().await?);
    let mut wrote = vec![];
    let mut fact = None;
    for _ in 0..3 {
        let ep = Episode::new(
            "note:bench",
            format!("bench probe {:?}", std::time::SystemTime::now()).as_bytes(),
            &["*"],
        );
        let f = Fact::new("bench", "probe", "x").from_episodes(&[&ep]);
        let t = std::time::Instant::now();
        writer
            .commit(scope, "bench", &[ep], std::slice::from_ref(&f))
            .await?;
        wrote.push(t.elapsed().as_millis());
        fact = Some(f);
    }
    let fact = fact.unwrap();
    let t = std::time::Instant::now();
    while !store
        .read(scope, &who, None)
        .await?
        .iter()
        .any(|f| f.fact_id == fact.fact_id)
    {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    println!(
        "3 commits took {wrote:?} ms (first is cold); last visible to another warm reader {} ms after commit",
        t.elapsed().as_millis()
    );
    Ok(())
}

/// One warm process for many searches (benchmarks): each input line is
/// {"id", "scope", "query", "k", "snippet_chars"?, "snippet_episodes"?}; each output line has the
/// hits, the best-matching passage of the top distinct source episodes, and the time. The searches run as user
/// `eval` in the groups `MEMSTORE_EVAL_GROUPS` names (comma-separated; none by default).
async fn search_batch(
    store: &Store,
    input: &std::path::Path,
    output: &std::path::Path,
) -> Result<()> {
    use std::io::{BufRead, Write};
    let groups = std::env::var("MEMSTORE_EVAL_GROUPS").unwrap_or_default();
    let groups: Vec<&str> = groups
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .collect();
    let who = Principal::new("eval", &groups);
    let mut out = std::io::BufWriter::new(std::fs::File::create(output)?);
    for line in std::io::BufReader::new(std::fs::File::open(input)?).lines() {
        let r: serde_json::Value = serde_json::from_str(&line?)?;
        let (scope, query) = (
            Scope::parse(r["scope"].as_str().unwrap_or_default())?,
            r["query"].as_str().unwrap_or_default(),
        );
        let t = std::time::Instant::now();
        let hits = memstore::search::search(
            store,
            &scope,
            memstore::MAIN,
            &who,
            query,
            r["k"].as_u64().unwrap_or(10) as usize,
            Some(i64::MAX),
        )
        .await?;
        let search_ms = t.elapsed().as_secs_f64() * 1e3;
        let (chars, eps) = (
            r["snippet_chars"].as_u64().unwrap_or(0) as usize,
            r["snippet_episodes"].as_u64().unwrap_or(3) as usize,
        );
        let mut snippets = serde_json::Map::new();
        if chars > 0 {
            for h in &hits {
                let Some(src) = h.fact.source_episodes.first() else {
                    continue;
                };
                if snippets.len() >= eps || snippets.contains_key(src) {
                    continue;
                }
                if let Some(b) = store.episode_body(&scope, &who, src).await? {
                    let text =
                        memstore::server::best_window(&String::from_utf8_lossy(&b), query, chars);
                    snippets.insert(src.clone(), serde_json::json!(text));
                }
            }
        }
        let hits: Vec<_> = hits
            .iter()
            .map(|h| {
                serde_json::json!({"value": h.fact.value, "entity": h.fact.entity,
                    "valid_from": h.fact.valid_from, "source": h.fact.source_episodes.first()})
            })
            .collect();
        writeln!(
            out,
            "{}",
            serde_json::json!({"id": r["id"], "hits": hits, "snippets": snippets,
                "search_ms": search_ms, "total_ms": t.elapsed().as_secs_f64() * 1e3})
        )?;
    }
    Ok(())
}

/// The store of a `serve --local` directory, opened where `serve --local` keeps its parts: the search index under
/// `DIR/index` and the vectors an index update computes in `DIR/embeddings` (unless MEMSTORE_EMBED_CACHE says
/// otherwise). Not the default index location (`~/.cache/memstore/index`), which every local store would share:
/// an index built there for one store was read, and overwritten, as another's.
async fn local_store(dir: &std::path::Path) -> Result<Store> {
    Ok(on_local(dir, memstore::platform::local_dir(dir).await?))
}

/// [`local_store`] on the catalog of `dir` already opened.
fn on_local(dir: &std::path::Path, catalog: std::sync::Arc<dyn iceberg::Catalog>) -> Store {
    memstore::search::embed_cache::default_to(dir.join("embeddings").display().to_string());
    Store::new(catalog).with_index_base(dir.join("index").display().to_string())
}

/// `memstore serve --local [DIR]`: everything under one directory on this machine. The one caller reads as
/// the groups given, else `local`; memories shared with `group:local` (or `*`) are theirs.
async fn serve_local(dir: &str, port: u16, scope: Scope, groups: Vec<String>) -> Result<()> {
    let dir = match dir.strip_prefix("~/") {
        Some(rest) => std::path::PathBuf::from(std::env::var("HOME")?).join(rest),
        None => std::path::PathBuf::from(dir),
    };
    // Every request reads as the one local caller, with no sign-in: never reachable from another machine.
    // The bind is fixed below whatever the environment says; a different one asked for is a mistake to report.
    if let Ok(bind) = std::env::var("MEMSTORE_SERVE_BIND") {
        anyhow::ensure!(
            bind == "127.0.0.1",
            "--local serves without sign-in, so it binds to 127.0.0.1 only (MEMSTORE_SERVE_BIND={bind})"
        );
    }
    let catalog = memstore::platform::local_dir(&dir).await?;
    // Unless MEMSTORE_EMBED_CACHE says otherwise, index updates keep the vectors they compute beside the index, so
    // the same text in another scope, workspace or branch of this store, or a rebuilt index, isn't embedded again.
    memstore::search::embed_cache::default_to(dir.join("embeddings").display().to_string());
    // Its store is a build (a benchmark's, a laptop's): the model calls it makes are kept in the model-call cache
    // builds share (`run` set it), so serving the same store rebuilt asks nothing twice.
    let groups = if groups.is_empty() {
        vec!["local".to_string()]
    } else {
        groups
    };
    eprintln!(
        "memstore: serving {} from {} on http://127.0.0.1:{port}/mcp (as group:{})",
        scope.namespace(),
        dir.display(),
        groups.join(", group:")
    );
    memstore::server::serve_with(
        port,
        scope,
        groups,
        memstore::server::Options {
            catalog: Some(catalog),
            identity: Some("fixed".into()),
            index_base: Some(dir.join("index").display().to_string()),
            bind: Some("127.0.0.1".into()),
            loopback_only: true,
            writes_here: true,
            ..Default::default()
        },
    )
    .await
}
