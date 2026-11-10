//! Applying a deployment plan in the customer's own cloud (`init` below: the apply step of a managed onboarding,
//! whose wizard is the hosted control plane's and whose CLI command, `memstore init`, is built only with the
//! `control-plane` feature; self-hosters use `deploy/gcp/terraform`, which this mirrors). [`Cmd`], [`Runner`] and
//! [`Shell`] are also what `memstore preflight` runs its read-only checks through.
//!
//! Each row of the plan is a resource: check whether it exists, adopt it once
//! it is shown compatible (or fix what isn't), else create it. Everything goes through `gcloud` and `bq` as the
//! person running `init`, with their own auth, and each command is printed before it runs, so they see exactly
//! what is done in their project. A [`Runner`] runs the commands: [`Shell`] for real, a fake in tests.
//!
//! The GCP rows mirror `deploy/gcp/terraform` (the production module): the same names, flags, environment and
//! grants, less what the plan doesn't list (the inbox, `remember`/`ask` and scheduled ingest).
//!
//! The bucket and the BigQuery dataset may be the customer's own ([`Existing`], `--existing-bucket` and
//! `--existing-dataset`), as the IAM templates in `deploy/iam` make them: then `init` only checks that each exists, is
//! readable and is as memstore needs it, and never creates, changes or grants on it; the customer's side does that.
//!
//! With `--iam-template <name>` the whole install runs on that template's vendor role: the template enabled the APIs,
//! made the engine's account and granted it, so `init` enables nothing, creates no account and makes no grant. It
//! checks each instead (enabled APIs, the account, the grants in the project's and the engine service's IAM
//! policies), and when one is missing stops with what the customer runs. The org, a write to the customer's
//! directory that the vendor role can't make, is left to the customer with the command that makes it.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

/// One command: a program (`gcloud`, `bq`, or `memstore` itself), its arguments, and environment it adds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Whether it changes anything: describes, lists and policy reads don't.
    pub mutates: bool,
}

impl Cmd {
    /// A command that changes nothing (a describe, a list, a policy read), for `memstore preflight`.
    pub fn read(program: &str, args: &[&str]) -> Self {
        Cmd::new(program, false, args)
    }

    fn new(program: &str, mutates: bool, args: &[&str]) -> Self {
        Cmd {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: vec![],
            mutates,
        }
    }
}

/// A shell word: as is when nothing in it is special, else single-quoted.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=@,+%".contains(&b));
    if plain {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// As the person would type it.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .chain(std::iter::once(self.program.clone()))
            .chain(self.args.iter().map(|a| quote(a)))
            .collect();
        f.write_str(&words.join(" "))
    }
}

/// What a command printed, and whether it succeeded.
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs commands: [`Shell`] for real, a fake in tests.
pub trait Runner {
    fn run(&mut self, cmd: &Cmd) -> Result<Output>;
}

/// The commands themselves, as the person running `init`: their own `gcloud` auth. `memstore` is this binary.
pub struct Shell;

impl Runner for Shell {
    fn run(&mut self, cmd: &Cmd) -> Result<Output> {
        let program = if cmd.program == crate::naming::BIN {
            std::env::current_exe()?
        } else {
            cmd.program.clone().into()
        };
        let out = std::process::Command::new(&program)
            .args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| anyhow::anyhow!("{}: {e} (is it installed and on PATH?)", cmd.program))?;
        Ok(Output {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// How a row ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    /// It existed, as the plan needs it.
    Adopted,
    Created,
    /// It existed and was changed to what the plan needs: what changed.
    Fixed(String),
    /// Not `init`'s to do as the vendor role: the customer's step, the command that does it.
    Customer(String),
}

impl Done {
    /// As the session's `rows` record it.
    pub fn status(&self) -> &'static str {
        match self {
            Done::Adopted => "adopted",
            Done::Created => "created",
            Done::Fixed(_) => "fixed",
            Done::Customer(_) => "not applied",
        }
    }
}

/// Where a row stopped: a short reason, and the command that fixes it or that the person can run.
#[derive(Debug)]
pub struct Failed {
    pub reason: String,
    pub fix: String,
}

/// The GCP rows, in the plan's order, before `Org` and `Deployment`.
pub const GCP_ROWS: [&str; 7] = [
    "Bucket",
    "BigLake catalog",
    "Service account",
    "Cloud Run service",
    "Cloud Run job",
    "BigQuery connection",
    "BigQuery functions",
];

/// The rows of the targets `init` can't apply yet, before the enrollment.
pub fn rows_not_yet(target: &str) -> Option<&'static [&'static str]> {
    Some(match target {
        "gke" => &["Namespace", "Deployment", "Bucket", "Org"],
        "spcs" => &["Application", "Compute pool", "Service", "Functions", "Org"],
        "databricks" => &["Catalog", "Serving endpoint", "Org"],
        _ => return None,
    })
}

/// The prefix of every name `init` gives (the IAM templates' `name`): `<name>-engine` is the service and its
/// account, `<name>-load` the job, `<name>` the connection.
pub const NAME: &str = "memstore";
/// The description `init` gives each routine it creates: in a dataset that isn't memstore's, it replaces only routines
/// carrying it. Unchanged by the rename: it is how routines an earlier `init` made are told apart.
pub const MARKER: &str = "Created by memstore init; replaced on each apply.";

/// The dataset `init` creates when the customer brings none (the IAM templates' `<name>_api`, name `memstore`).
pub const DATASET: &str = "memstore_api";
/// The template's custom roles (`deploy/iam/gcp/modules/runtime`) that `init --iam-template` checks the engine holds
/// on the project, besides `roles/bigquery.jobUser`; each is `<name with _>_<suffix>`.
const TEMPLATE_ENGINE_ROLES: [&str; 3] = ["engine_catalog", "model_predict", "bq_connection_use"];
/// The group every caller is in (the Terraform module's `audience` default).
const AUDIENCE: &str = "everyone";
/// The engine's project roles (`deploy/gcp/terraform`, `google_project_iam_member.engine`).
const ENGINE_ROLES: [&str; 3] = [
    "roles/biglake.editor",
    "roles/bigquery.jobUser",
    "roles/aiplatform.user",
];
/// The APIs each row needs, enabled before it is checked (a describe on a disabled API fails).
fn apis(kind: &str) -> &'static [&'static str] {
    match kind {
        "Bucket" => &["storage.googleapis.com"],
        "BigLake catalog" => &["biglake.googleapis.com"],
        "Service account" => &["iam.googleapis.com"],
        "Cloud Run service" | "Cloud Run job" => &["run.googleapis.com"],
        "BigQuery connection" => &[
            "bigquery.googleapis.com",
            "bigqueryconnection.googleapis.com",
            "aiplatform.googleapis.com",
        ],
        _ => &[],
    }
}

/// The remote functions: name, arguments, return type, the engine's path, rows per batch.
const FUNCTIONS: [(&str, &str, &str, &str, Option<u32>); 3] = [
    ("search", "q STRING, k INT64", "JSON", "/search", Some(50)),
    (
        "episode",
        "episode_id STRING, around STRING, max_chars INT64",
        "STRING",
        "/episode",
        Some(50),
    ),
    // The engine calls it once at start to learn BigQuery's caller id (AMPUP_MEMORY_WHOAMI_FUNCTION).
    ("_whoami", "nonce STRING", "STRING", "/whoami", None),
];

/// The customer's own bucket and dataset, to adopt rather than create (flags, else the plan's `existingBucket` and
/// `existingDataset` inputs), and the IAM template that made them (`--iam-template`, else the plan's
/// `iamTemplate`).
#[derive(Clone, Debug, Default)]
pub struct Existing {
    pub bucket: Option<String>,
    pub dataset: Option<String>,
    /// The `name` of the `deploy/iam/gcp` template applied in the project: run on its vendor role alone.
    pub iam_template: Option<String>,
}

/// A GCP plan: the wizard's inputs, checked, and what `init` adds.
#[derive(Clone, Debug)]
pub struct Gcp {
    pub project: String,
    pub region: String,
    pub bucket: String,
    /// The bucket is the customer's: checked, never created, changed or granted on.
    pub bucket_existing: bool,
    /// The BigQuery dataset the functions go in.
    pub dataset: String,
    /// The dataset is the customer's: checked, never created.
    pub dataset_existing: bool,
    /// The prefix of the names `init` gives ([`NAME`], or the IAM template's `name`).
    pub name: String,
    /// An IAM template made the APIs, the engine's account and its grants: run as its vendor role, which enables,
    /// creates and grants none of them; check them instead.
    pub vendor_only: bool,
    /// Names to adopt that only the plan gave (the control plane chose them, no flag here confirmed them): each
    /// as its flag and value. `init` adopts none of them unconfirmed.
    pub unconfirmed: Vec<(&'static str, String)>,
    /// The engine image; when the plan names none, the one an existing service runs.
    pub image: Option<String>,
    pub org: String,
    /// The org's first admin: who paired the session.
    pub admin: Option<String>,
}

fn valid(
    what: &str,
    v: &str,
    ok: impl Fn(u8) -> bool,
    len: std::ops::RangeInclusive<usize>,
) -> Result<String> {
    anyhow::ensure!(
        len.contains(&v.len()) && v.bytes().all(ok) && !v.starts_with('-'),
        "the plan's {what} is not a valid name: {v:?}"
    );
    Ok(v.into())
}

impl Gcp {
    /// From the session's plan (`{cloud, inputs: {project, region, bucket, existingBucket?, existingDataset?}}`)
    /// and `existing` (which wins over the plan), its names checked: they go into commands and SQL.
    pub fn from_plan(
        plan: &Value,
        org: &str,
        admin: Option<&str>,
        image: Option<&str>,
        existing: &Existing,
    ) -> Result<Self> {
        let input = |k: &str| {
            plan["inputs"][k]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        // A flag, else the plan's input (camelCase as the wizard writes it, or snake_case).
        let adopt = |flag: &Option<String>, camel: &str, snake: &str| {
            flag.as_deref()
                .map(|s| s.trim().to_string())
                .or_else(|| Some(input(camel)).filter(|s| !s.is_empty()))
                .or_else(|| Some(input(snake)).filter(|s| !s.is_empty()))
                .filter(|s| !s.is_empty())
        };
        let existing_bucket = adopt(&existing.bucket, "existingBucket", "existing_bucket");
        let existing_dataset = adopt(&existing.dataset, "existingDataset", "existing_dataset");
        let mut unconfirmed = vec![];
        if existing.bucket.is_none()
            && let Some(b) = &existing_bucket
        {
            unconfirmed.push(("--existing-bucket", b.clone()));
        }
        if existing.dataset.is_none()
            && let Some(d) = &existing_dataset
        {
            unconfirmed.push(("--existing-dataset", d.clone()));
        }
        let lower = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        // It renames which service accounts and resources init adopts and deploys as, so a name only the plan
        // gives is confirmed here like the bucket and dataset.
        let template = existing
            .iam_template
            .as_deref()
            .map(|s| s.trim().to_string())
            .or_else(|| Some(input("iamTemplate")).filter(|s| !s.is_empty()))
            .or_else(|| Some(input("iam_template")).filter(|s| !s.is_empty()))
            .filter(|s| !s.is_empty());
        if existing.iam_template.is_none()
            && let Some(t) = &template
        {
            unconfirmed.push(("--iam-template", t.clone()));
        }
        if let Some(t) = &template {
            // As deploy/iam/gcp validates its `name`.
            anyhow::ensure!(
                (3..=21).contains(&t.len())
                    && t.as_bytes()[0].is_ascii_lowercase()
                    && t.bytes().all(lower),
                "the IAM template's name is 3 to 21 lowercase letters, digits and dashes, starting with a letter: {t:?}"
            );
            anyhow::ensure!(
                existing_bucket.is_some() && existing_dataset.is_some(),
                "--iam-template {t} needs the template's bucket and dataset too (--existing-bucket, --existing-dataset): \
                 pass its whole init_flags output, from a template applied with cloud_run = true"
            );
        }
        let image = image
            .map(str::to_string)
            .or_else(|| plan["image"].as_str().map(str::to_string))
            .or_else(|| plan["inputs"]["image"].as_str().map(str::to_string))
            .filter(|i| !i.trim().is_empty());
        if let Some(i) = &image {
            valid(
                "image",
                i,
                |b| b.is_ascii_alphanumeric() || b"-_./:@".contains(&b),
                1..=512,
            )?;
        }
        Ok(Gcp {
            // Domain-scoped projects (example.com:p) too.
            project: valid(
                "project",
                &input("project"),
                |b| lower(b) || b == b'.' || b == b':',
                6..=63,
            )?,
            region: valid("region", &input("region"), lower, 2..=40)?,
            bucket: valid(
                "bucket",
                existing_bucket
                    .as_deref()
                    .unwrap_or(&input("bucket"))
                    .trim_start_matches("gs://"),
                |b| lower(b) || b == b'_' || b == b'.',
                3..=222,
            )?,
            bucket_existing: existing_bucket.is_some(),
            dataset: valid(
                "dataset",
                existing_dataset.as_deref().unwrap_or(DATASET),
                |b| b.is_ascii_alphanumeric() || b == b'_',
                1..=1024,
            )?,
            dataset_existing: existing_dataset.is_some(),
            vendor_only: template.is_some(),
            name: template.unwrap_or_else(|| NAME.into()),
            unconfirmed,
            image,
            org: org.into(),
            admin: admin.map(str::to_string),
        })
    }

    fn sa(&self) -> String {
        format!(
            "{}-engine@{}.iam.gserviceaccount.com",
            self.name, self.project
        )
    }

    /// The engine's Cloud Run service (and its account's id).
    fn service(&self) -> String {
        format!("{}-engine", self.name)
    }

    fn job(&self) -> String {
        format!("{}-load", self.name)
    }

    /// One of the IAM template's custom roles in this project.
    fn template_role(&self, suffix: &str) -> String {
        format!(
            "projects/{}/roles/{}_{suffix}",
            self.project,
            self.name.replace('-', "_")
        )
    }

    /// The IAM template's apply that makes what `init` found missing, by its owner (the vendor role can't), with
    /// `vars` added; then `retry`.
    fn template_fix(&self, vars: &str, retry: &str) -> String {
        format!(
            "terraform -chdir=deploy/iam/gcp/byoc apply -var project={} -var region={} -var name={} -var cloud_run=true \
             -var scheduled_ingest=false{vars}   # (or byovpc; with the -var flags it was applied with) by the project's \
             owner: the vendor role can't enable APIs, create accounts or grant; then {retry}",
            self.project, self.region, self.name
        )
    }

    fn gs(&self) -> String {
        format!("gs://{}", self.bucket)
    }

    fn connection(&self) -> String {
        format!("{}.{}.{}", self.project, self.region, self.name)
    }

    /// The name the wizard shows for each row.
    pub fn name(&self, kind: &str) -> String {
        match kind {
            "Bucket" => self.gs(),
            "BigLake catalog" => self.bucket.clone(),
            "Service account" => self.sa(),
            "Cloud Run service" => self.service(),
            "Cloud Run job" => self.job(),
            "BigQuery connection" => self.connection(),
            "BigQuery functions" => format!("{d}.search, {d}.episode", d = self.dataset),
            "Org" => self.org.clone(),
            _ => String::new(),
        }
    }

    // --- the commands, one place for apply and --dry-run ---------------------------------------------------------

    fn gcloud(&self, mutates: bool, args: &[&str]) -> Cmd {
        Cmd::new("gcloud", mutates, args)
    }

    fn bq(&self, mutates: bool, args: &[&str]) -> Cmd {
        Cmd::new("bq", mutates, args)
    }

    fn apis_list(&self) -> Cmd {
        self.gcloud(
            false,
            &[
                "services",
                "list",
                "--enabled",
                "--project",
                &self.project,
                "--format=json",
            ],
        )
    }

    fn apis_enable(&self, apis: &[String]) -> Cmd {
        let mut c = self.gcloud(true, &["services", "enable"]);
        c.args.extend(apis.iter().cloned());
        c.args.extend(["--project".into(), self.project.clone()]);
        c
    }

    fn bucket_describe(&self) -> Cmd {
        self.gcloud(
            false,
            &[
                "storage",
                "buckets",
                "describe",
                &self.gs(),
                "--project",
                &self.project,
                "--format=json",
            ],
        )
    }

    fn bucket_create(&self) -> Cmd {
        self.gcloud(
            true,
            &[
                "storage",
                "buckets",
                "create",
                &self.gs(),
                "--project",
                &self.project,
                "--location",
                &self.region,
                "--uniform-bucket-level-access",
                "--public-access-prevention",
            ],
        )
    }

    fn bucket_update(&self, uniform: bool, prevent: bool) -> Cmd {
        let mut c = self.gcloud(
            true,
            &[
                "storage",
                "buckets",
                "update",
                &self.gs(),
                "--project",
                &self.project,
            ],
        );
        if uniform {
            c.args.push("--uniform-bucket-level-access".into());
        }
        if prevent {
            c.args.push("--public-access-prevention".into());
        }
        c
    }

    fn catalog_describe(&self) -> Cmd {
        self.gcloud(
            false,
            &[
                "biglake",
                "iceberg",
                "catalogs",
                "describe",
                &self.bucket,
                "--project",
                &self.project,
                "--format=json",
            ],
        )
    }

    fn catalog_create(&self) -> Cmd {
        self.gcloud(
            true,
            &[
                "biglake",
                "iceberg",
                "catalogs",
                "create",
                &self.bucket,
                "--project",
                &self.project,
                "--catalog-type",
                "gcs-bucket",
                "--credential-mode",
                "end-user",
            ],
        )
    }

    fn sa_describe(&self) -> Cmd {
        self.gcloud(
            false,
            &[
                "iam",
                "service-accounts",
                "describe",
                &self.sa(),
                "--project",
                &self.project,
                "--format=json",
            ],
        )
    }

    fn sa_create(&self) -> Cmd {
        self.gcloud(
            true,
            &[
                "iam",
                "service-accounts",
                "create",
                &self.service(),
                "--project",
                &self.project,
                "--display-name",
                "AmpUp Memory engine (Cloud Run)",
            ],
        )
    }

    fn bucket_policy(&self) -> Cmd {
        self.gcloud(
            false,
            &[
                "storage",
                "buckets",
                "get-iam-policy",
                &self.gs(),
                "--project",
                &self.project,
                "--format=json",
            ],
        )
    }

    fn bucket_grant(&self, member: &str, role: &str) -> Cmd {
        self.gcloud(
            true,
            &[
                "storage",
                "buckets",
                "add-iam-policy-binding",
                &self.gs(),
                "--project",
                &self.project,
                "--member",
                member,
                "--role",
                role,
            ],
        )
    }

    fn project_policy(&self) -> Cmd {
        self.gcloud(
            false,
            &["projects", "get-iam-policy", &self.project, "--format=json"],
        )
    }

    fn project_grant(&self, member: &str, role: &str) -> Cmd {
        self.gcloud(
            true,
            &[
                "projects",
                "add-iam-policy-binding",
                &self.project,
                "--member",
                member,
                "--role",
                role,
                "--condition=None",
            ],
        )
    }

    fn run_args<'a>(&'a self, verb: &[&'a str]) -> Vec<&'a str> {
        let mut a = verb.to_vec();
        a.extend(["--project", &self.project, "--region", &self.region]);
        a
    }

    fn service_describe(&self) -> Cmd {
        let service = self.service();
        let mut a = self.run_args(&["run", "services", "describe", &service]);
        a.push("--format=json");
        self.gcloud(false, &a)
    }

    fn service_deploy(&self, image: &str) -> Cmd {
        let p = &self.project;
        // memstore's names (MEMSTORE_*): the plan's image may be from before the rename, which reads only these;
        // a newer engine reads them as AMPUP_MEMORY_* (memstore_client::naming). The job's below likewise.
        let env = [
            ("MEMSTORE_PLATFORM", "gcp".to_string()),
            ("GCP_PROJECT", p.clone()),
            ("GCP_REGION", self.region.clone()),
            ("GCS_BUCKET", self.bucket.clone()),
            ("MEMSTORE_SERVE_BIND", "0.0.0.0".into()),
            ("MEMSTORE_INDEX_URI", format!("gs://{}/_index", self.bucket)),
            ("MEMSTORE_CALLER_IDENTITY", "google".into()),
            (
                "MEMSTORE_WHOAMI_FUNCTION",
                format!("{p}.{}._whoami", self.dataset),
            ),
        ]
        .map(|(k, v)| format!("{k}={v}"))
        .join(",");
        let sa = self.sa();
        let args = format!("--args=serve,org,8080,{AUDIENCE}");
        let service = self.service();
        let mut a = self.run_args(&["run", "deploy", &service]);
        a.extend(["--image", image, "--service-account", &sa]);
        // That flag can set the service's IAM policy, which the vendor role can't: a new service is private without
        // it, and the policy is read right after either way.
        if !self.vendor_only {
            a.push("--no-allow-unauthenticated");
        }
        a.extend([
            &args,
            "--set-env-vars",
            &env,
            "--memory",
            "4Gi",
            "--cpu",
            "2",
            "--min-instances",
            "1",
            "--max-instances",
            "1",
            "--no-cpu-throttling",
            "--cpu-boost",
            "--quiet",
        ]);
        self.gcloud(true, &a)
    }

    fn service_update_sa(&self) -> Cmd {
        let sa = self.sa();
        let service = self.service();
        let mut a = self.run_args(&["run", "services", "update", &service]);
        a.extend(["--service-account", &sa, "--quiet"]);
        self.gcloud(true, &a)
    }

    fn service_policy(&self) -> Cmd {
        let service = self.service();
        let mut a = self.run_args(&["run", "services", "get-iam-policy", &service]);
        a.push("--format=json");
        self.gcloud(false, &a)
    }

    fn service_grant(&self, member: &str, role: &str) -> Cmd {
        let service = self.service();
        let mut a = self.run_args(&["run", "services", "add-iam-policy-binding", &service]);
        a.extend(["--member", member, "--role", role]);
        self.gcloud(true, &a)
    }

    fn service_ungrant(&self, member: &str, role: &str) -> Cmd {
        let service = self.service();
        let mut a = self.run_args(&["run", "services", "remove-iam-policy-binding", &service]);
        a.extend(["--member", member, "--role", role]);
        self.gcloud(true, &a)
    }

    fn job_describe(&self) -> Cmd {
        let job = self.job();
        let mut a = self.run_args(&["run", "jobs", "describe", &job]);
        a.push("--format=json");
        self.gcloud(false, &a)
    }

    fn job_create(&self, image: &str) -> Cmd {
        let sa = self.sa();
        let env = format!(
            "MEMSTORE_PLATFORM=gcp,GCP_PROJECT={},GCS_BUCKET={}",
            self.project, self.bucket
        );
        let volume = format!(
            "name=corpora,type=cloud-storage,bucket={},readonly=true",
            self.bucket
        );
        let job = self.job();
        let mut a = self.run_args(&["run", "jobs", "create", &job]);
        a.extend([
            "--image",
            image,
            "--service-account",
            &sa,
            "--memory",
            "8Gi",
            "--cpu",
            "4",
            "--task-timeout",
            "3600",
            "--max-retries",
            "0",
            "--set-env-vars",
            &env,
            "--add-volume",
            &volume,
            "--add-volume-mount",
            "volume=corpora,mount-path=/data",
            "--args=load,/data/_corpora/none,1000",
        ]);
        self.gcloud(true, &a)
    }

    fn job_update_sa(&self) -> Cmd {
        let sa = self.sa();
        let job = self.job();
        let mut a = self.run_args(&["run", "jobs", "update", &job]);
        a.extend(["--service-account", &sa]);
        self.gcloud(true, &a)
    }

    fn connection_show(&self) -> Cmd {
        self.bq(
            false,
            &["show", "--connection", "--format=json", &self.connection()],
        )
    }

    fn connection_create(&self) -> Cmd {
        self.bq(
            true,
            &[
                "mk",
                "--connection",
                "--project_id",
                &self.project,
                "--location",
                &self.region,
                "--connection_type",
                "CLOUD_RESOURCE",
                &self.name,
            ],
        )
    }

    fn connection_policy(&self) -> Cmd {
        self.bq(
            false,
            &[
                "get-iam-policy",
                "--connection",
                "--format=json",
                &self.connection(),
            ],
        )
    }

    fn connection_grant(&self, member: &str, role: &str) -> Cmd {
        self.bq(
            true,
            &[
                "add-iam-policy-binding",
                "--connection",
                "--member",
                member,
                "--role",
                role,
                &self.connection(),
            ],
        )
    }

    fn dataset_ref(&self) -> String {
        format!("{}:{}", self.project, self.dataset)
    }

    fn dataset_show(&self) -> Cmd {
        self.bq(false, &["show", "--format=json", &self.dataset_ref()])
    }

    /// Read access to the customer's dataset for `member` (the fix when it can't be read).
    fn dataset_grant_read(&self, member: &str) -> Cmd {
        self.bq(
            true,
            &[
                "add-iam-policy-binding",
                "--member",
                member,
                "--role",
                "roles/bigquery.metadataViewer",
                &self.dataset_ref(),
            ],
        )
    }

    /// Read access to the customer's bucket's metadata (`storage.buckets.get`, no objects) for `member`.
    fn bucket_grant_read(&self, member: &str) -> Cmd {
        self.bucket_grant(member, "roles/storage.bucketViewer")
    }

    /// Who `gcloud` runs as.
    fn account(&self) -> Cmd {
        self.gcloud(false, &["config", "get-value", "account"])
    }

    fn dataset_create(&self) -> Cmd {
        self.bq(
            true,
            &[
                "mk",
                "--dataset",
                "--location",
                &self.region,
                &self.dataset_ref(),
            ],
        )
    }

    fn function_show(&self, name: &str) -> Cmd {
        self.bq(
            false,
            &[
                "show",
                "--routine",
                "--format=json",
                &format!("{}.{name}", self.dataset_ref()),
            ],
        )
    }

    fn function_create(&self, f: &(&str, &str, &str, &str, Option<u32>), url: &str) -> Cmd {
        let (name, args, returns, path, batch) = f;
        let batching = batch
            .map(|n| format!(", max_batching_rows = {n}"))
            .unwrap_or_default();
        let sql = format!(
            "CREATE OR REPLACE FUNCTION `{p}.{d}.{name}`({args}) RETURNS {returns} REMOTE WITH CONNECTION \
             `{c}` OPTIONS (endpoint = '{url}{path}'{batching}, description = '{MARKER}')",
            p = self.project,
            d = self.dataset,
            c = self.connection(),
        );
        self.bq(
            true,
            &[
                "query",
                "--project_id",
                &self.project,
                "--location",
                &self.region,
                "--use_legacy_sql=false",
                &sql,
            ],
        )
    }

    fn org_env(&self) -> Vec<(String, String)> {
        [
            ("AMPUP_MEMORY_PLATFORM", "gcp"),
            ("GCP_PROJECT", &self.project),
            ("GCP_REGION", &self.region),
            ("GCS_BUCKET", &self.bucket),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn org_list(&self) -> Cmd {
        let mut c = Cmd::new(crate::naming::BIN, false, &["org", "list", "--direct"]);
        c.env = self.org_env();
        c
    }

    fn org_create(&self) -> Cmd {
        let mut c = Cmd::new(crate::naming::BIN, true, &["org", "create", &self.org]);
        if let Some(a) = &self.admin {
            c.args.extend(["--admin".into(), a.clone()]);
        }
        c.args.push("--direct".into());
        c.env = self.org_env();
        c
    }

    /// What each row checks and creates, for `--dry-run`: names the plan doesn't know yet are placeholders.
    pub fn commands(&self, kind: &str) -> Vec<Cmd> {
        let image = self
            .image
            .clone()
            .unwrap_or_else(|| "<engine image>".into());
        let sa = format!("serviceAccount:{}", self.sa());
        let csa = "serviceAccount:<the connection's service account>";
        let url = "<memstore-engine URL>";
        let mut c = vec![];
        let enable: Vec<String> = apis(kind).iter().map(|a| a.to_string()).collect();
        if !enable.is_empty() {
            c.push(self.apis_list());
            // With the IAM template, a missing API stops init with this command for the project's owner.
            if !self.vendor_only {
                c.push(self.apis_enable(&enable));
            }
        }
        if self.vendor_only {
            // Checks of what the template made; the creates are of what the vendor role may create.
            c.extend(match kind {
                "Bucket" => vec![self.bucket_describe()],
                "BigLake catalog" => vec![self.catalog_describe(), self.catalog_create()],
                "Service account" => vec![self.sa_describe(), self.project_policy()],
                "Cloud Run service" => vec![
                    self.service_describe(),
                    self.service_deploy(&image),
                    self.service_policy(),
                ],
                "Cloud Run job" => vec![self.job_describe(), self.job_create(&image)],
                "BigQuery connection" => vec![
                    self.connection_show(),
                    self.connection_create(),
                    self.service_policy(),
                    self.project_policy(),
                ],
                "BigQuery functions" => {
                    let mut v = vec![self.dataset_show()];
                    for f in &FUNCTIONS {
                        v.push(self.function_show(f.0));
                        v.push(self.function_create(f, url));
                    }
                    v
                }
                // The customer's: see [`Apply::org`].
                _ => vec![],
            });
            return c;
        }
        c.extend(match kind {
            "Bucket" if self.bucket_existing => vec![self.bucket_describe()],
            "Bucket" => vec![
                self.bucket_describe(),
                self.bucket_create(),
                self.bucket_update(true, true),
            ],
            "BigLake catalog" => vec![self.catalog_describe(), self.catalog_create()],
            "Service account" => {
                let mut v = vec![self.sa_describe(), self.sa_create()];
                if !self.bucket_existing {
                    v.push(self.bucket_policy());
                    v.push(self.bucket_grant(&sa, "roles/storage.objectAdmin"));
                }
                v.push(self.project_policy());
                v.extend(ENGINE_ROLES.iter().map(|r| self.project_grant(&sa, r)));
                v
            }
            "Cloud Run service" => vec![
                self.service_describe(),
                self.service_deploy(&image),
                self.service_policy(),
                self.service_grant(&sa, "roles/run.viewer"),
            ],
            "Cloud Run job" => vec![self.job_describe(), self.job_create(&image)],
            "BigQuery connection" => vec![
                self.connection_show(),
                self.connection_create(),
                self.service_policy(),
                self.service_grant(csa, "roles/run.invoker"),
                self.project_policy(),
                self.project_grant(csa, "roles/aiplatform.user"),
                self.connection_policy(),
                self.connection_grant(&sa, "roles/bigquery.connectionUser"),
            ],
            "BigQuery functions" => {
                let mut v = vec![self.dataset_show()];
                if !self.dataset_existing {
                    v.push(self.dataset_create());
                }
                for f in &FUNCTIONS {
                    v.push(self.function_show(f.0));
                    v.push(self.function_create(f, url));
                }
                v
            }
            "Org" => vec![self.org_list(), self.org_create()],
            _ => vec![],
        });
        c
    }
}

/// Whether a failed read said the thing isn't there (rather than, say, that it may not be read).
fn not_found(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "not found",
        "not_found",
        "notfound",
        "does not exist",
        "404",
    ]
    .iter()
    .any(|p| s.contains(p))
}

/// The gist of a failure: its last non-empty line, cut short.
fn gist(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("it failed without saying why");
    let mut s: String = line.chars().take(300).collect();
    if line.chars().count() > 300 {
        s.push('…');
    }
    s
}

/// Whether an IAM policy (`{bindings: [{role, members, condition?}]}`) grants `role` to `member` unconditionally.
fn grants(policy: &Value, member: &str, role: &str) -> bool {
    policy["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| {
            b["role"] == role
                && b["condition"].is_null()
                && b["members"]
                    .as_array()
                    .is_some_and(|m| m.iter().any(|x| x == member))
        })
}

/// Applies the GCP rows, one at a time, remembering what later rows need (the engine's URL and image, the
/// connection's service account).
pub struct Apply<'a> {
    runner: &'a mut dyn Runner,
    gcp: &'a Gcp,
    /// What retries `init` (`memstore init --apply <session>`), for fixes that are a re-run.
    retry: String,
    /// How long to wait between tries of a grant to a service account IAM doesn't know yet.
    pub retry_wait: Duration,
    enabled: Option<HashSet<String>>,
    service_url: Option<String>,
    service_image: Option<String>,
}

type Step<T> = std::result::Result<T, Failed>;

impl<'a> Apply<'a> {
    pub fn new(runner: &'a mut dyn Runner, gcp: &'a Gcp, retry: &str) -> Self {
        Apply {
            runner,
            gcp,
            retry: retry.into(),
            retry_wait: Duration::from_secs(5),
            enabled: None,
            service_url: None,
            service_image: None,
        }
    }

    /// The retry, less `flag` and its value (for a fix that gives the flag another value).
    fn retry_without(&self, flag: &str) -> String {
        let mut out: Vec<&str> = vec![];
        let mut words = self.retry.split(' ');
        while let Some(w) = words.next() {
            if w == flag {
                words.next();
            } else {
                out.push(w);
            }
        }
        out.join(" ")
    }

    fn run(&mut self, cmd: &Cmd) -> Step<Output> {
        println!("  $ {cmd}");
        self.runner.run(cmd).map_err(|e| Failed {
            reason: format!("{e:#}"),
            fix: cmd.to_string(),
        })
    }

    /// A read: its JSON, or None when the thing isn't there.
    fn read(&mut self, cmd: &Cmd) -> Step<Option<Value>> {
        let out = self.run(cmd)?;
        if out.ok {
            return Ok(Some(
                serde_json::from_str(&out.stdout).unwrap_or(Value::Null),
            ));
        }
        if not_found(&out.stderr) {
            return Ok(None);
        }
        Err(Failed {
            reason: gist(&out.stderr),
            fix: cmd.to_string(),
        })
    }

    /// A read of something the customer owns: its JSON, or None when it isn't there. When it can't be read, the
    /// fix is the grant (made by its owner) that lets whoever runs `init` read it: `grant` of their IAM member.
    fn read_theirs(
        &mut self,
        cmd: &Cmd,
        what: &str,
        grant: impl Fn(&str) -> Cmd,
    ) -> Step<Option<Value>> {
        let out = self.run(cmd)?;
        if out.ok {
            return Ok(Some(
                serde_json::from_str(&out.stdout).unwrap_or(Value::Null),
            ));
        }
        if not_found(&out.stderr) {
            return Ok(None);
        }
        let member = self.member();
        Err(Failed {
            reason: format!("{what} can't be read as {member}: {}", gist(&out.stderr)),
            fix: format!(
                "{}   # by its owner (the IAM templates in deploy/iam grant the vendor account this); then {}",
                grant(&member),
                self.retry
            ),
        })
    }

    /// The IAM member `gcloud` runs as, for a fix to name; a placeholder when it can't be told.
    fn member(&mut self) -> String {
        let cmd = self.gcp.account();
        let account = self
            .run(&cmd)
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.stdout.trim().to_string())
            .filter(|a| !a.is_empty() && !a.contains(char::is_whitespace));
        match account {
            Some(a) if a.ends_with(".gserviceaccount.com") => format!("serviceAccount:{a}"),
            Some(a) => format!("user:{a}"),
            None => "<the account running init>".into(),
        }
    }

    /// A change; when it fails, the fix is to run it (once what it says is put right).
    fn exec(&mut self, cmd: &Cmd) -> Step<()> {
        let out = self.run(cmd)?;
        if out.ok {
            return Ok(());
        }
        Err(Failed {
            reason: gist(&out.stderr),
            fix: cmd.to_string(),
        })
    }

    /// A grant to a service account made moments ago: IAM takes a few seconds to know it.
    fn exec_grant(&mut self, cmd: &Cmd) -> Step<()> {
        for _ in 0..12 {
            let out = self.run(cmd)?;
            if out.ok {
                return Ok(());
            }
            let s = out.stderr.to_ascii_lowercase();
            if !(s.contains("does not exist") || s.contains("not found")) {
                return Err(Failed {
                    reason: gist(&out.stderr),
                    fix: cmd.to_string(),
                });
            }
            std::thread::sleep(self.retry_wait);
        }
        Err(Failed {
            reason: "the service account is still unknown to IAM; try again in a minute".into(),
            fix: cmd.to_string(),
        })
    }

    fn ensure_apis(&mut self, kind: &str) -> Step<()> {
        let need = apis(kind);
        if need.is_empty() {
            return Ok(());
        }
        if self.enabled.is_none() {
            let cmd = self.gcp.apis_list();
            let v = self.read(&cmd)?.unwrap_or_default();
            self.enabled = Some(
                v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["config"]["name"].as_str().map(str::to_string))
                    .collect(),
            );
        }
        let enabled = self.enabled.as_mut().unwrap();
        let missing: Vec<String> = need
            .iter()
            .filter(|a| !enabled.contains(**a))
            .map(|a| a.to_string())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let cmd = self.gcp.apis_enable(&missing);
        if self.gcp.vendor_only {
            return Err(Failed {
                reason: format!(
                    "{} not enabled in {}; the IAM template enables them, and its vendor role can't",
                    missing.join(", "),
                    self.gcp.project
                ),
                fix: format!(
                    "{cmd}   # by the project's owner (or apply deploy/iam/gcp/byoc or byovpc with -var cloud_run=true); then {}",
                    self.retry
                ),
            });
        }
        self.exec(&cmd)?;
        self.enabled.as_mut().unwrap().extend(missing);
        Ok(())
    }

    /// Grants each of `roles` to `member` that `policy` (read by `read`) lacks; the roles granted.
    fn grant_missing(
        &mut self,
        read: Cmd,
        member: &str,
        roles: &[&str],
        grant: impl Fn(&str) -> Cmd,
    ) -> Step<Vec<String>> {
        let policy = self.read(&read)?.unwrap_or_default();
        let mut granted = vec![];
        for role in roles {
            if !grants(&policy, member, role) {
                self.exec_grant(&grant(role))?;
                granted.push(role.to_string());
            }
        }
        Ok(granted)
    }

    /// The roles of `want` (member, role) that `policy` (read by `read`) lacks, each as `role to member`.
    fn not_granted(&mut self, read: Cmd, want: &[(&str, String)]) -> Step<Vec<String>> {
        let policy = self.read(&read)?.unwrap_or_default();
        Ok(want
            .iter()
            .filter(|(m, r)| !grants(&policy, m, r))
            .map(|(m, r)| format!("{r} to {m}"))
            .collect())
    }

    /// Check one row, then adopt, fix or create it.
    pub fn row(&mut self, kind: &str) -> Step<Done> {
        self.ensure_apis(kind)?;
        match kind {
            "Bucket" => self.bucket(),
            "BigLake catalog" => self.catalog(),
            "Service account" => self.service_account(),
            "Cloud Run service" => self.service(),
            "Cloud Run job" => self.job(),
            "BigQuery connection" => self.connection(),
            "BigQuery functions" => self.functions(),
            "Org" => self.org(),
            _ => Err(Failed {
                reason: format!("init doesn't know how to apply {kind}"),
                fix: self.retry.clone(),
            }),
        }
    }

    fn bucket(&mut self) -> Step<Done> {
        let g = self.gcp;
        let b = if g.bucket_existing {
            let what = format!("{} (--existing-bucket)", g.gs());
            let Some(b) =
                self.read_theirs(&g.bucket_describe(), &what, |m| g.bucket_grant_read(m))?
            else {
                return Err(Failed {
                    reason: format!(
                        "{what} doesn't exist in {}; init doesn't create a bucket it was told is yours",
                        g.project
                    ),
                    fix: format!(
                        "{}   # by the project's owner, or apply deploy/iam/gcp/byoc (or byovpc) with -var bucket={}; then {}",
                        g.bucket_create(),
                        g.bucket,
                        self.retry
                    ),
                });
            };
            b
        } else {
            let Some(b) = self.read(&g.bucket_describe())? else {
                self.exec(&g.bucket_create())?;
                return Ok(Done::Created);
            };
            b
        };
        // gcloud storage's own shape, or the JSON API's.
        let uniform = b["uniform_bucket_level_access"].as_bool().unwrap_or(false)
            || b["iamConfiguration"]["uniformBucketLevelAccess"]["enabled"]
                .as_bool()
                .unwrap_or(false);
        let prevention = b["public_access_prevention"]
            .as_str()
            .or(b["iamConfiguration"]["publicAccessPrevention"].as_str())
            .unwrap_or_default();
        let prevented = prevention.eq_ignore_ascii_case("enforced");
        if uniform && prevented {
            return Ok(Done::Adopted);
        }
        if g.bucket_existing {
            // Not ours to change: its owner fixes it.
            return Err(Failed {
                reason: format!(
                    "{} (--existing-bucket) must have uniform bucket-level access and public access prevented",
                    g.gs()
                ),
                fix: format!(
                    "{}   # by the bucket's owner; then {}",
                    g.bucket_update(!uniform, !prevented),
                    self.retry
                ),
            });
        }
        self.exec(&g.bucket_update(!uniform, !prevented))?;
        let mut what = vec![];
        if !uniform {
            what.push("uniform access turned on");
        }
        if !prevented {
            what.push("public access prevented");
        }
        Ok(Done::Fixed(what.join(", ")))
    }

    fn catalog(&mut self) -> Step<Done> {
        let g = self.gcp;
        let Some(c) = self.read(&g.catalog_describe())? else {
            self.exec(&g.catalog_create())?;
            return Ok(Done::Created);
        };
        let mode = c["credentialMode"].as_str().unwrap_or_default();
        if !mode.is_empty() && !mode.to_ascii_uppercase().contains("END_USER") {
            return Err(Failed {
                reason: format!(
                    "catalog {} uses {mode}; the engine reads the bucket as its own service account (end-user \
                     credentials), and a catalog's credential mode can't be changed",
                    g.bucket
                ),
                fix: format!(
                    "gcloud biglake iceberg catalogs delete {} --project {}   # only if it holds no tables; then {}",
                    g.bucket, g.project, self.retry
                ),
            });
        }
        Ok(Done::Adopted)
    }

    /// With the IAM template: its engine account, and its grants on the project; nothing is created or granted.
    fn template_service_account(&mut self) -> Step<Done> {
        let g = self.gcp;
        let member = format!("serviceAccount:{}", g.sa());
        if self.read(&g.sa_describe())?.is_none() {
            return Err(Failed {
                reason: format!(
                    "{} doesn't exist; the IAM template {} makes it, and init doesn't create accounts with it",
                    g.sa(),
                    g.name
                ),
                fix: g.template_fix("", &self.retry),
            });
        }
        let mut want: Vec<(&str, String)> = TEMPLATE_ENGINE_ROLES
            .iter()
            .map(|r| (member.as_str(), g.template_role(r)))
            .collect();
        want.push((&member, "roles/bigquery.jobUser".into()));
        let missing = self.not_granted(g.project_policy(), &want)?;
        if !missing.is_empty() {
            return Err(Failed {
                reason: format!(
                    "the IAM template's grants aren't in {}'s policy: {}",
                    g.project,
                    missing.join("; ")
                ),
                fix: g.template_fix("", &self.retry),
            });
        }
        Ok(Done::Adopted)
    }

    fn service_account(&mut self) -> Step<Done> {
        let g = self.gcp;
        if g.vendor_only {
            return self.template_service_account();
        }
        let created = match self.read(&g.sa_describe())? {
            Some(_) => false,
            None => {
                self.exec(&g.sa_create())?;
                true
            }
        };
        let member = format!("serviceAccount:{}", g.sa());
        // The customer's bucket: its owner grants the engine object access (the IAM templates do).
        let mut granted = if g.bucket_existing {
            vec![]
        } else {
            self.grant_missing(
                g.bucket_policy(),
                &member,
                &["roles/storage.objectAdmin"],
                |r| g.bucket_grant(&member, r),
            )?
        };
        granted.extend(
            self.grant_missing(g.project_policy(), &member, &ENGINE_ROLES, |r| {
                g.project_grant(&member, r)
            })?,
        );
        Ok(match (created, granted.is_empty()) {
            (true, _) => Done::Created,
            (false, true) => Done::Adopted,
            (false, false) => Done::Fixed(format!("granted {}", granted.join(", "))),
        })
    }

    fn need_image(&self) -> Step<String> {
        self.gcp
            .image
            .clone()
            .or_else(|| self.service_image.clone())
            .ok_or_else(|| Failed {
                reason: "the plan names no engine image".into(),
                fix: format!("{} --image <engine image>", self.retry),
            })
    }

    fn service(&mut self) -> Step<Done> {
        let g = self.gcp;
        let sa = g.sa();
        let mut done = match self.read(&g.service_describe())? {
            Some(s) => {
                self.remember_service(&s);
                let runs_as = s["spec"]["template"]["spec"]["serviceAccountName"]
                    .as_str()
                    .unwrap_or_default();
                if runs_as == sa {
                    Done::Adopted
                } else {
                    self.exec(&g.service_update_sa())?;
                    Done::Fixed(format!("runs as {sa}"))
                }
            }
            None => {
                let image = self.need_image()?;
                self.exec(&g.service_deploy(&image))?;
                let s = self.read(&g.service_describe())?.unwrap_or_default();
                self.remember_service(&s);
                Done::Created
            }
        };
        // Private: the engine refuses callers while anyone at all may invoke it.
        let policy = self.read(&g.service_policy())?.unwrap_or_default();
        for public in ["allUsers", "allAuthenticatedUsers"] {
            if grants(&policy, public, "roles/run.invoker") {
                let ungrant = g.service_ungrant(public, "roles/run.invoker");
                return Err(Failed {
                    reason: format!(
                        "{} is public ({public} may invoke it); it must be private",
                        g.service()
                    ),
                    fix: if g.vendor_only {
                        format!(
                            "{ungrant}   # by the project's owner (the vendor role changes no IAM); then {}",
                            self.retry
                        )
                    } else {
                        ungrant.to_string()
                    },
                });
            }
        }
        // It reads its own service and IAM policy to trust Cloud Run-verified tokens. The IAM template grants
        // that on the service once it exists: checked with the connection's grants, below.
        let member = format!("serviceAccount:{sa}");
        if !g.vendor_only && !grants(&policy, &member, "roles/run.viewer") {
            self.exec_grant(&g.service_grant(&member, "roles/run.viewer"))?;
            if done == Done::Adopted {
                done = Done::Fixed("granted roles/run.viewer".into());
            }
        }
        if self.service_url.is_none() {
            return Err(Failed {
                reason: format!("{} has no URL yet", g.service()),
                fix: g.service_describe().to_string(),
            });
        }
        Ok(done)
    }

    fn remember_service(&mut self, s: &Value) {
        self.service_url = s["status"]["url"].as_str().map(str::to_string);
        self.service_image = s["spec"]["template"]["spec"]["containers"][0]["image"]
            .as_str()
            .map(str::to_string);
    }

    fn job(&mut self) -> Step<Done> {
        let g = self.gcp;
        match self.read(&g.job_describe())? {
            Some(j) => {
                let runs_as =
                    j["spec"]["template"]["spec"]["template"]["spec"]["serviceAccountName"]
                        .as_str()
                        .unwrap_or_default();
                if runs_as == g.sa() {
                    Ok(Done::Adopted)
                } else {
                    self.exec(&g.job_update_sa())?;
                    Ok(Done::Fixed(format!("runs as {}", g.sa())))
                }
            }
            None => {
                let image = self.need_image()?;
                self.exec(&g.job_create(&image))?;
                Ok(Done::Created)
            }
        }
    }

    fn connection(&mut self) -> Step<Done> {
        let g = self.gcp;
        let (c, created) = match self.read(&g.connection_show())? {
            Some(c) => (c, false),
            None => {
                self.exec(&g.connection_create())?;
                (self.read(&g.connection_show())?.unwrap_or_default(), true)
            }
        };
        let Some(csa) = c["cloudResource"]["serviceAccountId"].as_str() else {
            return Err(Failed {
                reason: format!("{} is not a Cloud resource connection", g.connection()),
                fix: format!(
                    "bq rm --connection {}   # then {}",
                    g.connection(),
                    self.retry
                ),
            });
        };
        let csa_email = csa.to_string();
        let csa = format!("serviceAccount:{csa}");
        let sa = format!("serviceAccount:{}", g.sa());
        if g.vendor_only {
            // The IAM template grants on the engine service and to the connection's account only once both exist
            // (its second apply): checked, never made here. The engine's use of the connection is a project grant,
            // checked with the account.
            let mut missing = self.not_granted(
                g.service_policy(),
                &[
                    (&sa, g.template_role("engine_run_self")),
                    (&csa, "roles/run.invoker".into()),
                ],
            )?;
            missing.extend(self.not_granted(
                g.project_policy(),
                &[(&csa, g.template_role("model_predict"))],
            )?);
            if !missing.is_empty() {
                return Err(Failed {
                    reason: format!(
                        "the IAM template hasn't granted on {} and to the connection's account yet: {}",
                        g.service(),
                        missing.join("; ")
                    ),
                    fix: g.template_fix(
                        &format!(
                            " -var cloud_run_deployed=true -var bigquery_connection_service_account={csa_email}"
                        ),
                        &self.retry,
                    ),
                });
            }
            return Ok(if created {
                Done::Created
            } else {
                Done::Adopted
            });
        }
        // BigQuery calls the engine, and Gemini; the engine calls its own _whoami through the connection.
        let mut granted =
            self.grant_missing(g.service_policy(), &csa, &["roles/run.invoker"], |r| {
                g.service_grant(&csa, r)
            })?;
        granted.extend(self.grant_missing(
            g.project_policy(),
            &csa,
            &["roles/aiplatform.user"],
            |r| g.project_grant(&csa, r),
        )?);
        granted.extend(self.grant_missing(
            g.connection_policy(),
            &sa,
            &["roles/bigquery.connectionUser"],
            |r| g.connection_grant(&sa, r),
        )?);
        Ok(match (created, granted.is_empty()) {
            (true, _) => Done::Created,
            (false, true) => Done::Adopted,
            (false, false) => Done::Fixed(format!("granted {}", granted.join(", "))),
        })
    }

    fn functions(&mut self) -> Step<Done> {
        let g = self.gcp;
        let Some(url) = self.service_url.clone() else {
            return Err(Failed {
                reason: format!("{}'s URL is unknown", g.service()),
                fix: self.retry.clone(),
            });
        };
        let mut created = false;
        let mut fixed = vec![];
        if g.dataset_existing {
            let what = format!("dataset {} (--existing-dataset)", g.dataset_ref());
            let Some(d) =
                self.read_theirs(&g.dataset_show(), &what, |m| g.dataset_grant_read(m))?
            else {
                return Err(Failed {
                    reason: format!(
                        "{what} doesn't exist; init doesn't create a dataset it was told is yours"
                    ),
                    fix: format!(
                        "{}   # by the project's owner, or apply deploy/iam/gcp/byoc (or byovpc) with -var cloud_run=true; then {}",
                        g.dataset_create(),
                        self.retry
                    ),
                });
            };
            // Remote functions live with their connection, in the plan's region.
            let location = d["location"].as_str().unwrap_or_default();
            if !location.is_empty() && !location.eq_ignore_ascii_case(&g.region) {
                return Err(Failed {
                    reason: format!(
                        "{what} is in {location}; its functions call the connection {}, in {}",
                        g.connection(),
                        g.region
                    ),
                    fix: format!(
                        "{} --existing-dataset <a dataset in {}>",
                        self.retry_without("--existing-dataset"),
                        g.region
                    ),
                });
            }
        } else if self.read(&g.dataset_show())?.is_none() {
            self.exec(&g.dataset_create())?;
            created = true;
        }
        for f in &FUNCTIONS {
            let want = format!("{url}{}", f.3);
            match self.read(&g.function_show(f.0))? {
                None => {
                    self.exec(&g.function_create(f, &url))?;
                    created = true;
                }
                // In the customer's dataset, a routine of that name that init didn't make is someone else's:
                // never replaced.
                Some(r) if g.dataset_existing && r["description"].as_str() != Some(MARKER) => {
                    let id = format!("{}.{}", g.dataset_ref(), f.0);
                    return Err(Failed {
                        reason: format!(
                            "routine {id} exists and AmpUp Memory didn't create it (its description isn't init's \
                             marker); init won't replace it"
                        ),
                        fix: format!(
                            "{} --existing-dataset <a dataset for AmpUp Memory alone>   # or, if {id} is AmpUp Memory's \
                             and may go: bq rm --routine {id}; then {}",
                            self.retry_without("--existing-dataset"),
                            self.retry
                        ),
                    });
                }
                Some(r)
                    if r["remoteFunctionOptions"]["endpoint"].as_str() != Some(want.as_str())
                        || r["description"].as_str() != Some(MARKER) =>
                {
                    self.exec(&g.function_create(f, &url))?;
                    fixed.push(f.0);
                }
                Some(_) => {}
            }
        }
        Ok(match (created, fixed.is_empty()) {
            (true, _) => Done::Created,
            (false, true) => Done::Adopted,
            (false, false) => Done::Fixed(format!("{} pointed at {url}", fixed.join(", "))),
        })
    }

    fn org(&mut self) -> Step<Done> {
        let g = self.gcp;
        if g.vendor_only {
            // Creating the org writes the customer's directory (tables in their bucket), which the vendor role can't
            // read or write by design: its owner runs it, as themselves.
            return Ok(Done::Customer(g.org_create().to_string()));
        }
        let orgs = self.read(&g.org_list())?.unwrap_or_default();
        let have: BTreeSet<&str> = orgs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if have.contains(g.org.as_str()) {
            return Ok(Done::Adopted);
        }
        self.exec(&g.org_create())?;
        Ok(Done::Created)
    }
}
