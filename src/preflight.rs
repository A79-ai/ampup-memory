//! `memstore preflight gcp`: whether a GCP project is ready for memstore, before anything is created in it.
//!
//! Read-only by construction: every command goes through [`crate::provision::Runner`] as [`crate::provision::Cmd`]s
//! that are never `mutates`, and the runner here refuses one that is. It reads the project with the admin's own
//! `gcloud` auth, prints each command before it runs, and writes nothing to the project and nothing to us: the
//! report goes to the terminal (or `--json` to stdout, for a file the customer may send).
//!
//! Each check ends PASS, WARN, FAIL or SKIP. FAIL is strict: the install won't work until it is fixed, and every
//! FAIL carries the exact fix. WARN says what to expect. SKIP says what the check couldn't read and why. The exit
//! code is 0 when everything passed, 1 with warnings only, 2 with any failure, 3 when the preflight itself couldn't
//! run (no `gcloud`, not signed in).
//!
//! The checks are one table, [`CHECKS`], and `--explain` prints it as the docs page (`docs/gcp/preflight.md`), so
//! the guide and the code can't drift. What they look for comes from what the IAM templates (`deploy/iam/gcp`) and
//! the plan's apply (`provision.rs`) already declare: the APIs, the names, the permissions.

use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::provision::{Cmd, Runner};

// The fixes that apply or enroll the install: `memstore init` too where it is built in (the `control-plane`
// feature).
const APPLY_AGAIN: &str = if cfg!(feature = "control-plane") {
    "apply deploy/gcp/terraform again, or ampup-memory init --apply"
} else {
    "apply deploy/gcp/terraform again"
};
const APPLY: &str = if cfg!(feature = "control-plane") {
    "ampup-memory init --apply <session>, or terraform apply in deploy/gcp/terraform"
} else {
    "terraform apply in deploy/gcp/terraform"
};
const ENROLL: &str = if cfg!(feature = "control-plane") {
    "the install enrolled: `ampup-memory init`, or `ampup-memory deployment enroll`; or --credential <file>"
} else {
    "the install enrolled: `ampup-memory deployment enroll`; or --credential <file>"
};

/// Which template the admin will apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    /// The vendor builds the network and the cluster (or Cloud Run) in the project.
    Byoc,
    /// The customer brings the VPC and a subnet.
    Byovpc,
}

/// Where the engine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Runtime {
    CloudRun,
    Gke,
}

/// What the admin told us about the install they intend.
#[derive(Clone, Debug)]
pub struct Target {
    pub project: String,
    pub region: String,
    pub variant: Variant,
    pub runtime: Runtime,
    /// The templates' `name`: the prefix of every resource.
    pub name: String,
    /// The memory bucket the template will create (default `<project>-memstore`).
    pub bucket: Option<String>,
    /// The vendor's GCP customer id (`C0…`), for the domain-restricted-sharing check.
    pub vendor_customer_id: Option<String>,
    /// The control plane the agent will reach (`https://…`).
    pub control_plane: Option<String>,
    /// The image repository's host (`us-docker.pkg.dev`).
    pub image_registry: Option<String>,
    /// `--installed`: where the image signatures live when not beside the image (the agent's
    /// `AMPUP_MEMORY_AGENT_SIGNATURE_REPOSITORY`).
    pub signature_repository: Option<String>,
    /// `--installed`: verify this image (`repository@sha256:…`) instead of the one the engine runs.
    pub image: Option<String>,
}

impl Target {
    fn bucket(&self) -> String {
        self.bucket
            .clone()
            .unwrap_or_else(|| format!("{}-memstore", self.project))
    }

    fn id(&self) -> String {
        self.name.replace('-', "_")
    }

    /// The APIs the templates enable (`deploy/iam/gcp/*/main.tf`, `google_project_service.apis`) plus the ones
    /// the plan's apply checks (`provision::apis`).
    fn apis(&self) -> Vec<&'static str> {
        let mut v = vec![
            "iam",
            "iamcredentials",
            "storage",
            "biglake",
            "aiplatform",
            "secretmanager",
            "compute",
            "serviceusage",
            "cloudresourcemanager",
        ];
        match self.runtime {
            Runtime::Gke => v.push("container"),
            Runtime::CloudRun => v.extend([
                "run",
                "bigquery",
                "bigqueryconnection",
                "cloudidentity",
                "cloudscheduler",
            ]),
        }
        v
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Outcome {
    Pass,
    Skip,
    Warn,
    Fail,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Outcome::Pass => "PASS",
            Outcome::Skip => "SKIP",
            Outcome::Warn => "WARN",
            Outcome::Fail => "FAIL",
        })
    }
}

/// One check's result.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub outcome: Outcome,
    /// What was found, in one line.
    pub found: String,
    /// What memstore needs, when it differs.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub required: String,
    /// The command or step that fixes it: present on every FAIL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Where to read more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<&'static str>,
}

impl Finding {
    fn new(id: &'static str, outcome: Outcome, found: impl Into<String>) -> Self {
        Finding {
            id,
            outcome,
            found: found.into(),
            required: String::new(),
            fix: None,
            doc: None,
        }
    }
    fn required(mut self, r: impl Into<String>) -> Self {
        self.required = r.into();
        self
    }
    fn fix(mut self, f: impl Into<String>) -> Self {
        self.fix = Some(f.into());
        self
    }
    fn doc(mut self, d: &'static str) -> Self {
        self.doc = Some(d);
        self
    }
}

/// A check as the docs describe it: what it reads, when it fails, how it is fixed.
pub struct Check {
    pub id: &'static str,
    pub reads: &'static str,
    pub fails_when: &'static str,
    pub fix: &'static str,
}

/// The table `--explain` prints. Ids match the findings'.
pub const CHECKS: &[Check] = &[
    Check {
        id: "tools",
        reads: "`gcloud --version`, `terraform version`",
        fails_when: "`gcloud` is missing (FAIL); `terraform` is missing or older than 1.5 (WARN: Cloud Shell has both)",
        fix: "install the Google Cloud CLI and Terraform, or run from Cloud Shell",
    },
    Check {
        id: "auth",
        reads: "`gcloud auth list`",
        fails_when: "no account is signed in",
        fix: "`gcloud auth login`",
    },
    Check {
        id: "project",
        reads: "`gcloud projects describe`",
        fails_when: "the project doesn't exist, can't be read, or isn't ACTIVE",
        fix: "check the id, or ask for Viewer on it",
    },
    Check {
        id: "billing",
        reads: "`gcloud billing projects describe`",
        fails_when: "no active billing account is linked (FAIL); it can't be read (SKIP: needs `billing.resourceAssociations.list`)",
        fix: "link a billing account in the console",
    },
    Check {
        id: "api",
        reads: "`gcloud services list --enabled`, against the APIs the template enables",
        fails_when: "an API is off and you can't enable it (FAIL); off but the template will enable it when you apply (WARN)",
        fix: "`gcloud services enable <api> --project <project>`",
    },
    Check {
        id: "org-policy",
        reads: "`gcloud resource-manager org-policies describe --effective`, one row per constraint",
        fails_when: "domain-restricted sharing excludes the vendor's customer id; `run.allowedIngress` forbids internal; `gcp.resourceLocations` excludes the region. External IPs or shielded VMs enforced are WARN (Cloud NAT and the template's node settings handle them); SA key creation disabled is PASS (AmpUp Memory makes none)",
        fix: "an exception on the project for the named constraint, by whoever administers org policies",
    },
    Check {
        id: "quota",
        reads: "`gcloud compute regions describe` (limit and usage) and `gcloud quotas info list` (limit) for the region",
        fails_when: "limit minus usage is below what the chosen size needs",
        fix: "request an increase from the console's Quotas page for the named quota, or choose another region",
    },
    Check {
        id: "names",
        reads: "the bucket, the engine's service account, the `<name>-engine` service, the `<name>_api` dataset and the vendor custom roles (including deleted ones)",
        fails_when: "the bucket name belongs to someone else (FAIL); a resource by that name already exists (WARN: `init` adopts it) or a custom role id was deleted in the last 37 days (WARN: choose another `name`)",
        fix: "another `-var bucket=` or `-var name=`",
    },
    Check {
        id: "egress",
        reads: "a TCP connect to the control plane and the image registry on 443, from this shell",
        fails_when: "a host can't be reached (WARN: this shell isn't the VPC; nodes need Cloud NAT or a proxy to the same hosts)",
        fix: "allow outbound 443 to the named hosts from the subnet",
    },
    Check {
        id: "region",
        reads: "`gcloud compute regions describe` (status) and `gcloud run regions list`",
        fails_when: "the region is DOWN, or Cloud Run doesn't serve it",
        fix: "`-var region=` another region",
    },
    Check {
        id: "permissions",
        reads: "`testIamPermissions` on the project for what applying the template needs",
        fails_when: "you lack one (WARN: the person who applies the template needs them all)",
        fix: "ask for Owner on the project, or Editor plus Project IAM Admin, Role Administrator and Service Account Admin",
    },
];

/// Org-policy constraints that have broken customer installs, and what each means for memstore.
const CONSTRAINTS: &[(&str, &str)] = &[
    (
        "iam.allowedPolicyMemberDomains",
        "domain-restricted sharing: the vendor principal must be allowed",
    ),
    (
        "iam.disableServiceAccountKeyCreation",
        "AmpUp Memory creates no service account key",
    ),
    (
        "iam.disableCrossProjectServiceAccountUsage",
        "AmpUp Memory attaches no service account from another project; the vendor impersonates one made here",
    ),
    (
        "compute.vmExternalIpAccess",
        "GKE nodes get no external IP; the template makes Cloud NAT",
    ),
    (
        "compute.requireShieldedVm",
        "the template's nodes are shielded",
    ),
    ("compute.restrictVpcPeering", "BYOC makes no peering"),
    (
        "compute.skipDefaultNetworkCreation",
        "the template makes its own network",
    ),
    ("run.allowedIngress", "the engine is `internal` ingress"),
    (
        "gcp.resourceLocations",
        "every resource is in the chosen region",
    ),
    (
        "storage.uniformBucketLevelAccess",
        "the bucket uses uniform access",
    ),
    (
        "storage.publicAccessPrevention",
        "the bucket prevents public access",
    ),
];

/// What applying `deploy/iam/gcp/{byoc,byovpc}` needs on the project.
const TEMPLATE_PERMISSIONS: &[&str] = &[
    "resourcemanager.projects.setIamPolicy",
    "serviceusage.services.enable",
    "iam.roles.create",
    "iam.roles.update",
    "iam.serviceAccounts.create",
    "iam.serviceAccounts.setIamPolicy",
    "storage.buckets.create",
    "storage.buckets.setIamPolicy",
    "bigquery.datasets.create",
    "compute.networks.get",
];

/// What one size needs, for the quota rows.
struct Size {
    /// Cloud Run: vCPU for the engine plus its jobs.
    run_vcpu: u64,
    /// GKE: N2 vCPUs, persistent SSD GB, in-use addresses.
    n2_cpus: f64,
    ssd_gb: f64,
    addresses: f64,
}

/// The team size (`docs/gcp/sizing.md`): a 2 vCPU engine, a 4 vCPU load job and a 1 vCPU ingest job, or three
/// n2-standard-4 nodes.
const SIZE: Size = Size {
    run_vcpu: 7,
    n2_cpus: 12.0,
    ssd_gb: 300.0,
    addresses: 2.0,
};

/// Runs the commands and refuses one that mutates.
struct ReadOnly<'a> {
    inner: &'a mut dyn Runner,
    quiet: bool,
}

impl ReadOnly<'_> {
    fn run(&mut self, cmd: &Cmd) -> Result<crate::provision::Output> {
        anyhow::ensure!(
            !cmd.mutates,
            "preflight refuses to run a command that changes anything: {cmd}"
        );
        if !self.quiet {
            eprintln!("  $ {cmd}");
        }
        self.inner.run(cmd)
    }

    /// Runs a command and parses its stdout as JSON; `Ok(None)` when it failed (stderr in the error text).
    fn json(&mut self, cmd: &Cmd) -> Result<std::result::Result<Value, String>> {
        let out = self.run(cmd)?;
        if !out.ok {
            return Ok(Err(gist(&out.stderr)));
        }
        Ok(serde_json::from_str(&out.stdout)
            .map_err(|e| format!("unreadable output from {}: {e}", cmd.program)))
    }
}

/// The first line of stderr that says something, without gcloud's "ERROR: (gcloud.x.y)" prefix.
fn gist(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)");
    let line = line.strip_prefix("ERROR:").unwrap_or(line).trim();
    let line = match line.strip_prefix('(') {
        Some(rest) => rest.split_once(')').map(|(_, r)| r.trim()).unwrap_or(line),
        None => line,
    };
    line.split(" This command is authenticated as")
        .next()
        .unwrap_or(line)
        .trim()
        .to_string()
}

fn gcloud(args: &[&str]) -> Cmd {
    Cmd::read("gcloud", args)
}

/// The whole report.
#[derive(Serialize)]
pub struct Report {
    pub project: String,
    pub region: String,
    pub variant: String,
    pub runtime: String,
    pub account: Option<String>,
    pub at: String,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn worst(&self) -> Outcome {
        self.findings
            .iter()
            .map(|f| f.outcome)
            .max()
            .unwrap_or(Outcome::Pass)
    }

    /// The process exit code: 0 all pass, 1 warnings, 2 failures, 3 when the preflight itself couldn't run (no
    /// gcloud, nobody signed in, the project unreadable).
    pub fn exit_code(&self) -> i32 {
        if self
            .findings
            .iter()
            .any(|f| matches!(f.id, "tools" | "auth" | "project") && f.outcome == Outcome::Fail)
        {
            return 3;
        }
        match self.worst() {
            Outcome::Pass | Outcome::Skip => 0,
            Outcome::Warn => 1,
            Outcome::Fail => 2,
        }
    }

    fn count(&self, o: Outcome) -> usize {
        self.findings.iter().filter(|f| f.outcome == o).count()
    }

    /// The terminal report.
    pub fn print(&self, version: &str) {
        println!(
            "ampup-memory preflight {}, project {}, region {}, {} on {}, as {}, {}",
            version,
            self.project,
            self.region,
            self.variant,
            self.runtime,
            self.account.as_deref().unwrap_or("nobody"),
            self.at
        );
        println!();
        for f in &self.findings {
            println!(" {:<4}  {:<12} {}", f.outcome, f.id, f.found);
            if !f.required.is_empty() {
                println!("                    needs: {}", f.required);
            }
            if let Some(fix) = &f.fix {
                println!("                    fix: {fix}");
            }
            if let Some(doc) = f.doc {
                println!("                    see: {doc}");
            }
        }
        println!();
        let (fail, warn, pass, skip) = (
            self.count(Outcome::Fail),
            self.count(Outcome::Warn),
            self.count(Outcome::Pass),
            self.count(Outcome::Skip),
        );
        let mut line = format!("{fail} FAIL, {warn} WARN, {pass} PASS");
        if skip > 0 {
            line.push_str(&format!(", {skip} SKIP"));
        }
        line.push_str(match self.worst() {
            Outcome::Fail => ". Fix the FAIL rows and run again. Nothing was created.",
            Outcome::Warn => ". Read the WARN rows; the install can go ahead. Nothing was created.",
            _ => ". Ready. Nothing was created.",
        });
        println!("{line}");
    }
}

/// `--explain`: the checks as a Markdown page.
pub fn explain() -> String {
    let mut s = String::from(
        "# What `ampup-memory preflight gcp` checks\n\n\
         Generated by `ampup-memory preflight gcp --explain`; edit `src/preflight.rs`, not this file.\n\n\
         Every check is read-only: it runs `gcloud` as you, prints each command first, and creates, changes and \
         sends nothing. Outcomes: **FAIL** stops the install until fixed and always carries the fix; **WARN** says \
         what to expect; **PASS** says what was found; **SKIP** says what couldn't be read. Exit code 0 all pass, \
         1 warnings only, 2 any failure, 3 the preflight itself couldn't run. `--json` writes the same rows as \
         `{id, outcome, found, required, fix, doc}`.\n\n\
         | Check | Reads | FAIL or WARN when | Fix |\n|---|---|---|---|\n",
    );
    for c in CHECKS {
        s.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            c.id, c.reads, c.fails_when, c.fix
        ));
    }
    s.push_str("\n## Org-policy constraints read\n\n| Constraint | What it means for AmpUp Memory |\n|---|---|\n");
    for (c, why) in CONSTRAINTS {
        s.push_str(&format!("| `{c}` | {why} |\n"));
    }
    s.push_str("\n## Permissions the template's applier needs\n\n");
    for p in TEMPLATE_PERMISSIONS {
        s.push_str(&format!("- `{p}`\n"));
    }
    s.push_str(&format!(
        "\n## Quota the team size needs\n\n\
         Cloud Run: {} vCPU of `run.googleapis.com/cpu_allocation` in the region. \
         GKE: {} `N2_CPUS`, {} `SSD_TOTAL_GB`, {} `IN_USE_ADDRESSES` in the region.\n",
        SIZE.run_vcpu, SIZE.n2_cpus, SIZE.ssd_gb, SIZE.addresses
    ));
    s
}

/// Runs every check. `quiet` hides the commands as they run (for `--json`).
pub async fn run(t: &Target, runner: &mut dyn Runner, quiet: bool) -> Result<Report> {
    let mut r = ReadOnly {
        inner: runner,
        quiet,
    };
    let mut findings = vec![];
    let mut account = None;

    // tools
    match r.run(&Cmd::read("gcloud", &["--version"])) {
        Ok(o) if o.ok => {
            let g = o
                .stdout
                .lines()
                .find(|l| l.starts_with("Google Cloud SDK"))
                .unwrap_or("Google Cloud SDK")
                .to_string();
            let tf = r.run(&Cmd::read("terraform", &["version", "-json"]));
            let (outcome, found) = match tf {
                Ok(o) if o.ok => {
                    let v: Value = serde_json::from_str(&o.stdout).unwrap_or_default();
                    let ver = v["terraform_version"].as_str().unwrap_or("?").to_string();
                    let old = ver
                        .split('.')
                        .next()
                        .and_then(|m| m.parse::<u32>().ok())
                        .is_some_and(|m| m < 1);
                    (
                        if old { Outcome::Warn } else { Outcome::Pass },
                        format!("{g}, terraform {ver}"),
                    )
                }
                _ => (
                    Outcome::Warn,
                    format!(
                        "{g}; terraform not found (the templates need >= 1.5; Cloud Shell has it)"
                    ),
                ),
            };
            findings
                .push(Finding::new("tools", outcome, found).doc("https://cloud.google.com/shell"));
        }
        _ => {
            findings.push(
                Finding::new("tools", Outcome::Fail, "gcloud not found on PATH")
                    .fix("install the Google Cloud CLI, or run this from Cloud Shell")
                    .doc("https://cloud.google.com/sdk/docs/install"),
            );
            return Ok(report(t, account, findings));
        }
    }

    // auth
    match r.json(&gcloud(&[
        "auth",
        "list",
        "--filter=status:ACTIVE",
        "--format=json",
    ]))? {
        Ok(v) => match v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["account"].as_str())
        {
            Some(a) => {
                account = Some(a.to_string());
                findings.push(Finding::new(
                    "auth",
                    Outcome::Pass,
                    format!("signed in as {a}"),
                ));
            }
            None => {
                findings.push(
                    Finding::new("auth", Outcome::Fail, "no account is signed in to gcloud")
                        .fix("gcloud auth login"),
                );
                return Ok(report(t, account, findings));
            }
        },
        Err(e) => {
            findings.push(Finding::new("auth", Outcome::Fail, e).fix("gcloud auth login"));
            return Ok(report(t, account, findings));
        }
    }

    // project
    match r.json(&gcloud(&[
        "projects",
        "describe",
        &t.project,
        "--format=json(lifecycleState,projectNumber,parent)",
    ]))? {
        Ok(v) => {
            let state = v["lifecycleState"].as_str().unwrap_or("?");
            let parent = match (v["parent"]["type"].as_str(), v["parent"]["id"].as_str()) {
                (Some(ty), Some(id)) => format!(", in {ty} {id}"),
                _ => ", with no parent (no org policies apply)".into(),
            };
            let found = format!(
                "{} is {state}, number {}{parent}",
                t.project,
                v["projectNumber"].as_str().unwrap_or("?")
            );
            if state == "ACTIVE" {
                findings.push(Finding::new("project", Outcome::Pass, found));
            } else {
                findings.push(
                    Finding::new("project", Outcome::Fail, found)
                        .required("an ACTIVE project")
                        .fix("restore the project, or use another"),
                );
                return Ok(report(t, account, findings));
            }
        }
        Err(e) => {
            findings.push(Finding::new("project", Outcome::Fail, e).fix(format!(
                "check the project id, or ask for roles/viewer on {}",
                t.project
            )));
            return Ok(report(t, account, findings));
        }
    }

    // billing
    match r.json(&gcloud(&[
        "billing",
        "projects",
        "describe",
        &t.project,
        "--format=json",
    ]))? {
        Ok(v) if v["billingEnabled"].as_bool() == Some(true) => findings.push(Finding::new(
            "billing",
            Outcome::Pass,
            format!(
                "billing account {} is linked",
                v["billingAccountName"]
                    .as_str()
                    .unwrap_or("?")
                    .trim_start_matches("billingAccounts/")
            ),
        )),
        Ok(_) => findings.push(
            Finding::new("billing", Outcome::Fail, "no active billing account is linked")
                .required("billing on: BigQuery, Cloud Run and GCS refuse a project without it")
                .fix(format!(
                    "gcloud billing projects link {} --billing-account <ACCOUNT_ID>",
                    t.project
                ))
                .doc("https://cloud.google.com/billing/docs/how-to/modify-project"),
        ),
        Err(e) => findings.push(
            Finding::new("billing", Outcome::Skip, format!("couldn't read billing: {e}"))
                .required("someone with billing.resourceAssociations.list confirms a billing account is linked"),
        ),
    }

    // api
    let enabled: Vec<String> = match r.json(&gcloud(&[
        "services",
        "list",
        "--enabled",
        "--project",
        &t.project,
        "--format=json(config.name)",
    ]))? {
        Ok(v) => v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["config"]["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            findings.push(
                Finding::new(
                    "api",
                    Outcome::Fail,
                    format!("couldn't list enabled APIs: {e}"),
                )
                .required("serviceusage.services.list on the project (roles/viewer has it)")
                .fix(
                    "gcloud services enable serviceusage.googleapis.com --project ".to_string()
                        + &t.project,
                ),
            );
            vec![]
        }
    };
    let permissions = test_permissions(&mut r, &t.project).await;
    let can_enable = permissions
        .as_ref()
        .map(|p| p.iter().any(|x| x == "serviceusage.services.enable"))
        .unwrap_or(true);
    if !enabled.is_empty() {
        let missing: Vec<String> = t
            .apis()
            .iter()
            .map(|a| format!("{a}.googleapis.com"))
            .filter(|a| !enabled.contains(a))
            .collect();
        if missing.is_empty() {
            findings.push(Finding::new(
                "api",
                Outcome::Pass,
                format!("all {} APIs the install needs are enabled", t.apis().len()),
            ));
        } else {
            let fix = format!(
                "gcloud services enable {} --project {}",
                missing.join(" "),
                t.project
            );
            let f = if can_enable {
                Finding::new(
                    "api",
                    Outcome::Warn,
                    format!(
                        "not enabled: {} (the template enables them when you apply)",
                        missing.join(", ")
                    ),
                )
            } else {
                Finding::new(
                    "api",
                    Outcome::Fail,
                    format!(
                        "not enabled: {}, and you can't enable APIs here",
                        missing.join(", ")
                    ),
                )
                .required("serviceusage.services.enable, or someone who has it runs the fix")
            };
            findings.push(
                f.fix(fix)
                    .doc("https://cloud.google.com/service-usage/docs/enable-disable"),
            );
        }
    }

    // org-policy
    for (constraint, why) in CONSTRAINTS {
        let v = match r.json(&gcloud(&[
            "resource-manager",
            "org-policies",
            "describe",
            constraint,
            "--project",
            &t.project,
            "--effective",
            "--format=json",
        ]))? {
            Ok(v) => v,
            Err(e) => {
                findings.push(
                    Finding::new("org-policy", Outcome::Skip, format!("{constraint}: couldn't read ({e})"))
                        .required("orgpolicy.policy.get on the project, or the project's org-policy admin reads it"),
                );
                continue;
            }
        };
        findings.push(org_policy(t, constraint, why, &v));
    }

    // quota
    match r.json(&gcloud(&[
        "compute",
        "regions",
        "describe",
        &t.region,
        "--project",
        &t.project,
        "--format=json(status,quotas)",
    ]))? {
        Ok(v) => {
            let status = v["status"].as_str().unwrap_or("?");
            findings.push(if status == "UP" {
                Finding::new("region", Outcome::Pass, format!("{} is UP", t.region))
            } else {
                Finding::new("region", Outcome::Fail, format!("{} is {status}", t.region))
                    .fix("-var region=<another region>")
            });
            if t.runtime == Runtime::Gke {
                for (metric, need) in [
                    ("N2_CPUS", SIZE.n2_cpus),
                    ("SSD_TOTAL_GB", SIZE.ssd_gb),
                    ("IN_USE_ADDRESSES", SIZE.addresses),
                ] {
                    let q = v["quotas"]
                        .as_array()
                        .and_then(|a| a.iter().find(|q| q["metric"].as_str() == Some(metric)));
                    findings.push(match q {
                        Some(q) => {
                            let limit = q["limit"].as_f64().unwrap_or(0.0);
                            let used = q["usage"].as_f64().unwrap_or(0.0);
                            let gap = need - (limit - used);
                            let found = format!(
                                "compute {metric} {}: limit {limit}, used {used}, required {need}",
                                t.region
                            );
                            if gap > 0.0 {
                                Finding::new("quota", Outcome::Fail, format!("{found}, gap {gap}"))
                                    .fix(format!(
                                        "request {metric} >= {} in {} (console: IAM & Admin > Quotas), or -var region=<another>",
                                        used + need,
                                        t.region
                                    ))
                                    .doc("https://cloud.google.com/docs/quotas/view-manage")
                            } else {
                                Finding::new("quota", Outcome::Pass, found)
                            }
                        }
                        None => Finding::new("quota", Outcome::Skip, format!("compute {metric}: not reported for {}", t.region)),
                    });
                }
            }
        }
        Err(e) => findings.push(
            Finding::new(
                "region",
                Outcome::Skip,
                format!("couldn't read {}: {e}", t.region),
            )
            .required("compute.googleapis.com enabled and compute.regions.get"),
        ),
    }
    if t.runtime == Runtime::CloudRun {
        match r.json(&gcloud(&[
            "quotas",
            "info",
            "list",
            "--service=run.googleapis.com",
            "--project",
            &t.project,
            "--format=json(quotaId,dimensionsInfos)",
        ]))? {
            Ok(v) => {
                let limit = v
                    .as_array()
                    .and_then(|a| {
                        a.iter()
                            .find(|q| q["quotaId"].as_str() == Some("CpuAllocPerProjectRegion"))
                    })
                    .and_then(|q| q["dimensionsInfos"].as_array())
                    .and_then(|d| {
                        d.iter().find(|d| {
                            d["applicableLocations"]
                                .as_array()
                                .is_some_and(|l| l.iter().any(|x| x.as_str() == Some(&t.region)))
                        })
                    })
                    .and_then(|d| d["details"]["value"].as_str())
                    .and_then(|s| s.parse::<u64>().ok());
                // The API reports the limit in millicores (1000 per vCPU) and no usage.
                let need = SIZE.run_vcpu * 1000;
                findings.push(match limit {
                    Some(l) if l >= need => Finding::new(
                        "quota",
                        Outcome::Pass,
                        format!(
                            "run.googleapis.com/cpu_allocation {}: limit {} vCPU, the team size needs {} (usage isn't reported by the API)",
                            t.region,
                            l / 1000,
                            SIZE.run_vcpu
                        ),
                    ),
                    Some(l) => Finding::new(
                        "quota",
                        Outcome::Fail,
                        format!("run.googleapis.com/cpu_allocation {}: limit {} vCPU, required {}", t.region, l / 1000, SIZE.run_vcpu),
                    )
                    .fix(format!("request run.googleapis.com/cpu_allocation >= {} vCPU in {} (console: IAM & Admin > Quotas)", SIZE.run_vcpu, t.region))
                    .doc("https://cloud.google.com/run/quotas"),
                    None => Finding::new("quota", Outcome::Skip, format!("run.googleapis.com/cpu_allocation: no limit reported for {}", t.region)),
                });
                let regions = v
                    .as_array()
                    .and_then(|a| {
                        a.iter()
                            .find(|q| q["quotaId"].as_str() == Some("CpuAllocPerProjectRegion"))
                    })
                    .and_then(|q| q["dimensionsInfos"].as_array())
                    .map(|d| {
                        d.iter()
                            .flat_map(|d| {
                                d["applicableLocations"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                            })
                            .any(|x| x.as_str() == Some(&t.region))
                    });
                if regions == Some(false) {
                    findings.push(
                        Finding::new(
                            "region",
                            Outcome::Fail,
                            format!("Cloud Run has no quota in {}", t.region),
                        )
                        .fix("-var region=<a Cloud Run region>")
                        .doc("https://cloud.google.com/run/docs/locations"),
                    );
                }
            }
            Err(e) => findings.push(
                Finding::new(
                    "quota",
                    Outcome::Skip,
                    format!("couldn't read Cloud Run quota: {e}"),
                )
                .required("cloudquotas.quotas.get (roles/viewer has it) and the Cloud Quotas API"),
            ),
        }
    }

    // names
    let bucket = t.bucket();
    let out = r.run(&gcloud(&[
        "storage",
        "buckets",
        "describe",
        &format!("gs://{bucket}"),
        "--format=json(name,location,versioning)",
    ]))?;
    findings.push(if out.ok {
        let v: Value = serde_json::from_str(&out.stdout).unwrap_or_default();
        Finding::new(
            "names",
            Outcome::Warn,
            format!(
                "bucket {bucket} exists in a project you can read ({}): the install adopts it with --existing-bucket",
                v["location"].as_str().unwrap_or("?")
            ),
        )
    } else if out.stderr.contains("404") || out.stderr.contains("not found") {
        Finding::new("names", Outcome::Pass, format!("bucket {bucket} is free"))
    } else if out.stderr.contains("403") || out.stderr.contains("does not have") {
        Finding::new("names", Outcome::Fail, format!("bucket {bucket} belongs to someone else"))
            .fix("-var bucket=<another globally unique name>")
    } else {
        Finding::new("names", Outcome::Skip, format!("bucket {bucket}: {}", gist(&out.stderr)))
    });
    let sa = format!("{}-engine@{}.iam.gserviceaccount.com", t.name, t.project);
    let out = r.run(&gcloud(&[
        "iam",
        "service-accounts",
        "describe",
        &sa,
        "--project",
        &t.project,
        "--format=json(email)",
    ]))?;
    findings.push(if out.ok {
        Finding::new(
            "names",
            Outcome::Warn,
            format!("service account {sa} exists: the install adopts it"),
        )
    } else if out.stderr.contains("NOT_FOUND") {
        Finding::new(
            "names",
            Outcome::Pass,
            format!("service account {sa} is free"),
        )
    } else {
        Finding::new(
            "names",
            Outcome::Skip,
            format!("service account {sa}: {}", gist(&out.stderr)),
        )
    });
    match r.json(&gcloud(&[
        "iam",
        "roles",
        "list",
        "--project",
        &t.project,
        "--show-deleted",
        "--format=json(name,deleted)",
    ]))? {
        Ok(v) => {
            let prefix = format!("projects/{}/roles/{}_", t.project, t.id());
            let (deleted, live): (Vec<&Value>, Vec<&Value>) = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|r| r["name"].as_str().is_some_and(|n| n.starts_with(&prefix)))
                        .partition(|r| r["deleted"].as_bool() == Some(true))
                })
                .unwrap_or_default();
            let names = |rs: &[&Value]| {
                rs.iter()
                    .filter_map(|r| r["name"].as_str()?.rsplit('/').next())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !deleted.is_empty() {
                findings.push(
                    Finding::new(
                        "names",
                        Outcome::Warn,
                        format!(
                            "custom roles deleted recently keep their id for 37 days: {}",
                            names(&deleted)
                        ),
                    )
                    .fix("-var name=<another prefix>, or wait")
                    .doc("https://cloud.google.com/iam/docs/creating-custom-roles#deleting-custom-role"),
                );
            }
            if !live.is_empty() {
                findings.push(Finding::new(
                    "names",
                    Outcome::Warn,
                    format!(
                        "the template's custom roles exist: {} (a re-apply updates them)",
                        names(&live)
                    ),
                ));
            }
            if deleted.is_empty() && live.is_empty() {
                findings.push(Finding::new(
                    "names",
                    Outcome::Pass,
                    format!("no custom role named {}_* exists or is reserved", t.id()),
                ));
            }
        }
        Err(e) => findings.push(Finding::new(
            "names",
            Outcome::Skip,
            format!("custom roles: {e}"),
        )),
    }
    if t.runtime == Runtime::CloudRun {
        let service = format!("{}-engine", t.name);
        let out = r.run(&gcloud(&[
            "run",
            "services",
            "describe",
            &service,
            "--project",
            &t.project,
            "--region",
            &t.region,
            "--format=json(metadata.name)",
        ]))?;
        findings.push(if out.ok {
            Finding::new(
                "names",
                Outcome::Warn,
                format!(
                    "Cloud Run service {service} exists in {}: the install adopts it",
                    t.region
                ),
            )
        } else if ["not be found", "NOT_FOUND", "not found", "Cannot find"]
            .iter()
            .any(|m| out.stderr.contains(m))
        {
            Finding::new(
                "names",
                Outcome::Pass,
                format!("Cloud Run service {service} is free"),
            )
        } else {
            Finding::new(
                "names",
                Outcome::Skip,
                format!("Cloud Run service {service}: {}", gist(&out.stderr)),
            )
        });
    }

    // egress
    for (what, url) in [
        ("control plane", t.control_plane.as_deref()),
        ("image registry", t.image_registry.as_deref()),
    ] {
        let Some(url) = url else {
            findings.push(Finding::new(
                "egress",
                Outcome::Skip,
                format!(
                    "{what} not given (--{}): reachability not checked",
                    what.replace(' ', "-")
                ),
            ));
            continue;
        };
        let host = url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(url)
            .to_string();
        let host_port = if host.contains(':') {
            host.clone()
        } else {
            format!("{host}:443")
        };
        findings.push(match reach(&host_port) {
            Ok(()) => Finding::new("egress", Outcome::Pass, format!("{what} {host}:443 reachable from this shell (nodes need the same through Cloud NAT or a proxy)")),
            Err(e) => Finding::new("egress", Outcome::Warn, format!("{what} {host}:443 not reachable from this shell: {e}"))
                .required(format!("outbound 443 to {host} from the engine's subnet"))
                .fix(format!("allow egress to {host}:443 (firewall, proxy or Cloud NAT), then run again from a VM in the subnet")),
        });
    }

    // permissions
    match &permissions {
        Some(have) => {
            let missing: Vec<&str> = TEMPLATE_PERMISSIONS
                .iter()
                .copied()
                .filter(|p| !have.iter().any(|h| h == p))
                .collect();
            findings.push(if missing.is_empty() {
                Finding::new(
                    "permissions",
                    Outcome::Pass,
                    format!(
                        "{} may apply the template ({} of {} permissions)",
                        account.as_deref().unwrap_or("you"),
                        TEMPLATE_PERMISSIONS.len(),
                        TEMPLATE_PERMISSIONS.len()
                    ),
                )
            } else {
                Finding::new(
                    "permissions",
                    Outcome::Warn,
                    format!(
                        "{} lacks {} of {} permissions the template's apply needs: {}",
                        account.as_deref().unwrap_or("you"),
                        missing.len(),
                        TEMPLATE_PERMISSIONS.len(),
                        missing.join(", ")
                    ),
                )
                .required("whoever applies deploy/iam/gcp has them all: Owner, or Editor plus Project IAM Admin, Role Administrator and Service Account Admin")
                .fix(format!(
                    "gcloud projects add-iam-policy-binding {} --member user:<who applies> --role roles/owner",
                    t.project
                ))
                .doc("https://cloud.google.com/iam/docs/understanding-roles")
            });
        }
        None => findings.push(
            Finding::new(
                "permissions",
                Outcome::Skip,
                "couldn't test permissions on the project",
            )
            .required(
                "an access token from gcloud, and cloudresourcemanager.googleapis.com reachable",
            ),
        ),
    }

    Ok(report(t, account, findings))
}

/// `--installed`: the same report, against the running install (Cloud Run today; GKE rows are SKIP).
pub async fn run_installed(
    t: &Target,
    runner: &mut dyn Runner,
    quiet: bool,
    cosign_key: Option<&Path>,
    credential: Option<&Path>,
) -> Result<Report> {
    let mut r = ReadOnly {
        inner: runner,
        quiet,
    };
    let mut findings = vec![];
    let account = match r.json(&gcloud(&[
        "auth",
        "list",
        "--filter=status:ACTIVE",
        "--format=json",
    ]))? {
        Ok(v) => v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["account"].as_str())
            .map(str::to_string),
        Err(_) => None,
    };
    if account.is_none() {
        findings.push(
            Finding::new("auth", Outcome::Fail, "no account is signed in to gcloud")
                .fix("gcloud auth login"),
        );
        return Ok(report(t, account, findings));
    }
    findings.push(Finding::new(
        "auth",
        Outcome::Pass,
        format!("signed in as {}", account.as_deref().unwrap_or("")),
    ));

    // engine
    let service = format!("{}-engine", t.name);
    let sa = format!("{}-engine@{}.iam.gserviceaccount.com", t.name, t.project);
    let mut url = None;
    let mut digest = None;
    if t.runtime == Runtime::Gke {
        findings.push(Finding::new(
            "engine",
            Outcome::Skip,
            "GKE: the installed checks read Cloud Run today",
        ));
    } else {
        match r.json(&gcloud(&[
            "run",
            "services",
            "describe",
            &service,
            "--project",
            &t.project,
            "--region",
            &t.region,
            "--format=json(metadata.annotations,spec.template.spec.serviceAccountName,spec.template.spec.containers,status.url,status.conditions,status.latestReadyRevisionName)",
        ]))? {
            Ok(v) => {
                let ready = v["status"]["conditions"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"));
                let image = v["spec"]["template"]["spec"]["containers"][0]["image"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string();
                let runs_as = v["spec"]["template"]["spec"]["serviceAccountName"]
                    .as_str()
                    .unwrap_or("the default compute account");
                let ingress = v["metadata"]["annotations"]["run.googleapis.com/ingress"]
                    .as_str()
                    .unwrap_or("all");
                url = v["status"]["url"].as_str().map(str::to_string);
                let found = format!("{service} in {} runs {image} as {runs_as}, ingress {ingress}", t.region);
                if !ready {
                    findings.push(
                        Finding::new("engine", Outcome::Fail, format!("{found}: not Ready"))
                            .fix(format!("gcloud run services logs read {service} --project {} --region {}", t.project, t.region)),
                    );
                } else if runs_as != sa || ingress != "internal" {
                    findings.push(
                        Finding::new("engine", Outcome::Warn, found)
                            .required(format!("runs as {sa}, ingress internal"))
                            .fix(APPLY_AGAIN),
                    );
                } else {
                    findings.push(Finding::new("engine", Outcome::Pass, found));
                }
                if let Some(rev) = v["status"]["latestReadyRevisionName"].as_str()
                    && let Ok(Ok(rv)) = r.json(&gcloud(&[
                        "run",
                        "revisions",
                        "describe",
                        rev,
                        "--project",
                        &t.project,
                        "--region",
                        &t.region,
                        "--format=json(status.imageDigest)",
                    ])) {
                        digest = rv["status"]["imageDigest"].as_str().map(str::to_string);
                    }
            }
            Err(e) => {
                findings.push(
                    Finding::new("engine", Outcome::Fail, format!("{service}: {e}"))
                        .fix(APPLY),
                );
            }
        }

        // private
        match r.json(&gcloud(&[
            "run",
            "services",
            "get-iam-policy",
            &service,
            "--project",
            &t.project,
            "--region",
            &t.region,
            "--format=json",
        ]))? {
            Ok(v) => {
                let invokers: Vec<String> = v["bindings"]
                    .as_array()
                    .map(|b| {
                        b.iter()
                            .filter(|b| b["role"] == "roles/run.invoker")
                            .flat_map(|b| b["members"].as_array().cloned().unwrap_or_default())
                            .filter_map(|m| m.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let public: Vec<&str> = invokers
                    .iter()
                    .map(String::as_str)
                    .filter(|m| *m == "allUsers" || *m == "allAuthenticatedUsers")
                    .collect();
                findings.push(if public.is_empty() {
                    Finding::new(
                        "private",
                        Outcome::Pass,
                        format!("{} invoker(s), none public: the engine refuses anyone else", invokers.len()),
                    )
                } else {
                    Finding::new("private", Outcome::Fail, format!("{} may invoke the engine", public.join(" and ")))
                        .fix(format!(
                            "gcloud run services remove-iam-policy-binding {service} --member {} --role roles/run.invoker --project {} --region {}",
                            public[0], t.project, t.region
                        ))
                });
            }
            Err(e) => findings.push(Finding::new(
                "private",
                Outcome::Skip,
                format!("couldn't read the service's IAM policy: {e}"),
            )),
        }

        // health
        if let Some(u) = &url {
            let token = r
                .run(&gcloud(&["auth", "print-identity-token"]))
                .ok()
                .filter(|o| o.ok)
                .map(|o| o.stdout.trim().to_string());
            let health = format!("{u}/health");
            if !quiet {
                eprintln!("  GET {health}");
            }
            let resp = match token {
                Some(tk) => {
                    reqwest::Client::builder()
                        .timeout(Duration::from_secs(30))
                        .build()?
                        .get(&health)
                        .bearer_auth(tk)
                        .send()
                        .await
                }
                None => Err(reqwest::Client::new()
                    .get("http://invalid")
                    .build()
                    .unwrap_err()),
            };
            findings.push(match resp {
                Ok(x) if x.status().is_success() => Finding::new("health", Outcome::Pass, format!("{health} answered {}", x.status())),
                Ok(x) if x.status().as_u16() == 403 || x.status().as_u16() == 401 => Finding::new(
                    "health",
                    Outcome::Skip,
                    format!("{health} answered {}: you aren't an invoker of the engine", x.status()),
                )
                .required("roles/run.invoker on the service, or an invoker runs this"),
                Ok(x) => Finding::new("health", Outcome::Fail, format!("{health} answered {}", x.status()))
                    .fix(format!("gcloud run services logs read {service} --project {} --region {}", t.project, t.region)),
                Err(e) => Finding::new("health", Outcome::Warn, format!("{health}: {e}"))
                    .required("the engine's URL reachable from this shell (internal ingress needs a VPC path)"),
            });
        }

        // signature
        let digest = t.image.clone().or(digest);
        match (cosign_key, &digest) {
            (Some(key), Some(d)) => {
                let key = crate::cosign::PublicKey::from_pem_file(key)?;
                let (repo, dg) = d.split_once('@').unwrap_or((d, ""));
                let sig_repo = t.signature_repository.as_deref().unwrap_or(repo);
                // From this shell the registry is read with gcloud's token, where the agent uses the metadata server's.
                let token = r
                    .run(&gcloud(&["auth", "print-access-token"]))
                    .ok()
                    .filter(|o| o.ok)
                    .map(|o| o.stdout.trim().to_string());
                let reg =
                    crate::cosign::Registry::new(sig_repo, None, None, None)?.with_token(token);
                if !quiet {
                    eprintln!(
                        "  GET {sig_repo}:sha256-….sig (cosign signature, key {})",
                        key.fingerprint()
                    );
                }
                findings.push(match reg.verify(repo, dg, &key).await {
                    Ok(()) => Finding::new("signature", Outcome::Pass, format!("{d} is signed by the key {}", key.fingerprint())),
                    Err(crate::cosign::Refusal::Unreachable(e)) => Finding::new("signature", Outcome::Skip, format!("{sig_repo}: couldn't read its signatures ({e:#})"))
                        .required("artifactregistry.repositories.downloadArtifacts on the repository"),
                    Err(e) => Finding::new("signature", Outcome::Fail, format!("{d}: {}", e.code()))
                        .required(format!("a cosign signature by the key {}", key.fingerprint()))
                        .fix("the vendor rolls a signed image; the agent refuses to upgrade to an unsigned one too"),
                });
            }
            (None, _) => findings.push(Finding::new(
                "signature",
                Outcome::Skip,
                "no --cosign-key given: the running image's signature isn't checked",
            )),
            (_, None) => findings.push(Finding::new(
                "signature",
                Outcome::Skip,
                "no revision digest to check",
            )),
        }
    }

    // data
    let bucket = t.bucket();
    match r.json(&gcloud(&[
        "storage",
        "buckets",
        "describe",
        &format!("gs://{bucket}"),
        "--format=json(versioning,iamConfiguration)",
    ]))? {
        Ok(v) => {
            let versioning = v["versioning"]["enabled"].as_bool() == Some(true);
            let uniform = v["iamConfiguration"]["uniformBucketLevelAccess"]["enabled"].as_bool()
                == Some(true);
            let pap = v["iamConfiguration"]["publicAccessPrevention"].as_str() == Some("enforced");
            let public = match r.json(&gcloud(&[
                "storage",
                "buckets",
                "get-iam-policy",
                &format!("gs://{bucket}"),
                "--format=json",
            ]))? {
                Ok(p) => p["bindings"].as_array().is_some_and(|b| {
                    b.iter().any(|b| {
                        b["members"].as_array().is_some_and(|m| {
                            m.iter()
                                .any(|m| m == "allUsers" || m == "allAuthenticatedUsers")
                        })
                    })
                }),
                Err(_) => false,
            };
            let found = format!(
                "bucket {bucket}: versioning {}, uniform access {}, public-access prevention {}",
                if versioning { "on" } else { "off" },
                if uniform { "on" } else { "off" },
                if pap { "enforced" } else { "inherited" }
            );
            findings.push(if public || !uniform {
                Finding::new("data", Outcome::Fail, format!("{found}{}", if public { ", public" } else { "" }))
                    .fix(format!("gcloud storage buckets update gs://{bucket} --uniform-bucket-level-access --public-access-prevention, and remove any allUsers binding"))
            } else if !versioning || !pap {
                Finding::new("data", Outcome::Warn, found)
                    .fix(format!("gcloud storage buckets update gs://{bucket} --versioning --public-access-prevention"))
            } else {
                Finding::new("data", Outcome::Pass, found)
            });
        }
        Err(e) => findings.push(
            Finding::new("data", Outcome::Fail, format!("bucket {bucket}: {e}"))
                .fix("apply the IAM template, which creates it"),
        ),
    }
    if t.runtime == Runtime::CloudRun {
        let dataset = format!("{}_api", t.id());
        match r.json(&Cmd::read(
            "bq",
            &["show", "--format=json", &format!("{}:{dataset}", t.project)],
        ))? {
            Ok(v) => {
                let public = v["access"].as_array().is_some_and(|a| {
                    a.iter().any(|e| {
                        matches!(e["specialGroup"].as_str(), Some("allAuthenticatedUsers"))
                            || e["iamMember"] == "allUsers"
                    })
                });
                findings.push(if public {
                    Finding::new("data", Outcome::Fail, format!("dataset {dataset} is readable by everyone"))
                        .fix(format!("remove allAuthenticatedUsers from the access list of {}:{dataset} (console: BigQuery > Sharing)", t.project))
                } else {
                    Finding::new("data", Outcome::Pass, format!("dataset {dataset} has no public access entry"))
                });
            }
            Err(e) => findings.push(
                Finding::new("data", Outcome::Warn, format!("dataset {dataset}: {e}")).required(
                    "the `<name>_api` dataset, from the IAM template with cloud_run = true",
                ),
            ),
        }
    }

    // enrolled
    let path = credential
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::deployment::Credential::default_path);
    match crate::deployment::Credential::load(&path) {
        Ok(c) => {
            if !quiet {
                eprintln!(
                    "  GET {}/v1/whoami (signed as {})",
                    c.control_plane,
                    c.deployment.as_str()
                );
            }
            findings.push(match c.call("GET", "/v1/whoami", &Value::Null).await {
                Ok((200, v)) => Finding::new(
                    "enrolled",
                    Outcome::Pass,
                    format!(
                        "deployment {} is enrolled with {}{}",
                        c.deployment.as_str(),
                        c.control_plane,
                        v["org"]
                            .as_str()
                            .map(|o| format!(" for org {o}"))
                            .unwrap_or_default()
                    ),
                ),
                Ok((status, v)) => Finding::new(
                    "enrolled",
                    Outcome::Fail,
                    format!(
                        "{} answered {status} for deployment {}: {}",
                        c.control_plane,
                        c.deployment.as_str(),
                        v["error"].as_str().unwrap_or("")
                    ),
                )
                .fix(
                    "ampup-memory deployment enroll --token <a new enrollment token from the console>",
                ),
                Err(e) => Finding::new(
                    "enrolled",
                    Outcome::Warn,
                    format!("{}: {e:#}", c.control_plane),
                )
                .required("outbound 443 to the control plane from here"),
            });
        }
        Err(_) => findings.push(
            Finding::new(
                "enrolled",
                Outcome::Skip,
                format!("no deployment credential at {}", path.display()),
            )
            .required(ENROLL),
        ),
    }

    Ok(report(t, account, findings))
}

/// The strict checks a plan's apply runs before it creates anything: the project is readable and
/// active, billing is on, the region is up. Prints each row; `Err` on a FAIL.
pub fn gate(project: &str, region: &str, runner: &mut dyn Runner) -> Result<()> {
    let mut r = ReadOnly {
        inner: runner,
        quiet: true,
    };
    let mut findings = vec![];
    match r.json(&gcloud(&[
        "projects",
        "describe",
        project,
        "--format=json(lifecycleState)",
    ]))? {
        Ok(v) if v["lifecycleState"] == "ACTIVE" => findings.push(Finding::new(
            "project",
            Outcome::Pass,
            format!("{project} is ACTIVE"),
        )),
        Ok(v) => findings.push(
            Finding::new(
                "project",
                Outcome::Fail,
                format!(
                    "{project} is {}",
                    v["lifecycleState"].as_str().unwrap_or("?")
                ),
            )
            .fix("restore the project, or use another"),
        ),
        Err(e) => findings.push(Finding::new("project", Outcome::Fail, e).fix(format!(
            "check the project id, or ask for roles/viewer on {project}"
        ))),
    }
    match r.json(&gcloud(&[
        "billing",
        "projects",
        "describe",
        project,
        "--format=json",
    ]))? {
        Ok(v) if v["billingEnabled"] == true => {
            findings.push(Finding::new("billing", Outcome::Pass, "billing is on"))
        }
        Ok(_) => findings.push(
            Finding::new(
                "billing",
                Outcome::Fail,
                "no active billing account is linked",
            )
            .fix(format!(
                "gcloud billing projects link {project} --billing-account <ACCOUNT_ID>"
            )),
        ),
        Err(e) => findings.push(Finding::new(
            "billing",
            Outcome::Skip,
            format!("couldn't read billing: {e}"),
        )),
    }
    match r.json(&gcloud(&[
        "compute",
        "regions",
        "describe",
        region,
        "--project",
        project,
        "--format=json(status)",
    ]))? {
        Ok(v) if v["status"] == "UP" => findings.push(Finding::new(
            "region",
            Outcome::Pass,
            format!("{region} is UP"),
        )),
        Ok(v) => findings.push(
            Finding::new(
                "region",
                Outcome::Fail,
                format!("{region} is {}", v["status"].as_str().unwrap_or("?")),
            )
            .fix("choose another region in the wizard"),
        ),
        Err(e) => findings.push(Finding::new(
            "region",
            Outcome::Skip,
            format!("couldn't read {region}: {e}"),
        )),
    }
    let mut failed = vec![];
    for f in &findings {
        println!("  {} {:<8} {}", f.outcome, f.id, f.found);
        if f.outcome == Outcome::Fail {
            if let Some(fix) = &f.fix {
                println!("                fix: {fix}");
            }
            failed.push(format!("{}: {}", f.id, f.found));
        }
    }
    anyhow::ensure!(
        failed.is_empty(),
        "preflight failed: {} (ampup-memory preflight gcp --project {project} --region {region} says more)",
        failed.join("; ")
    );
    Ok(())
}

fn report(t: &Target, account: Option<String>, findings: Vec<Finding>) -> Report {
    Report {
        project: t.project.clone(),
        region: t.region.clone(),
        variant: format!("{:?}", t.variant).to_lowercase(),
        runtime: match t.runtime {
            Runtime::CloudRun => "cloud-run".into(),
            Runtime::Gke => "gke".into(),
        },
        account,
        at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        findings,
    }
}

/// One effective org-policy constraint, read in the v1 shape `gcloud resource-manager org-policies` prints.
fn org_policy(t: &Target, constraint: &'static str, why: &str, v: &Value) -> Finding {
    let boolean = v["booleanPolicy"]["enforced"].as_bool() == Some(true);
    let list = &v["listPolicy"];
    let all = list["allValues"].as_str();
    let allowed: Vec<&str> = list["allowedValues"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let denied: Vec<&str> = list["deniedValues"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let unset = !boolean && all.is_none() && allowed.is_empty() && denied.is_empty();
    let fix = |what: &str| {
        format!(
            "an org-policy exception on {} for constraints/{constraint} ({what}), by whoever administers org policies",
            t.project
        )
    };
    let doc =
        "https://cloud.google.com/resource-manager/docs/organization-policy/org-policy-constraints";
    match constraint {
        "iam.allowedPolicyMemberDomains" => {
            if unset || all == Some("ALLOW") {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: any domain may be granted"),
                )
            } else {
                let id = t.vendor_customer_id.as_deref();
                let ok = id.is_some_and(|id| {
                    allowed
                        .iter()
                        .any(|a| *a == id || a.trim_start_matches("is:") == id)
                });
                if ok {
                    Finding::new(
                        "org-policy",
                        Outcome::Pass,
                        format!(
                            "{constraint}: the vendor's customer id {} is allowed",
                            id.unwrap_or("")
                        ),
                    )
                } else {
                    Finding::new(
                        "org-policy",
                        Outcome::Fail,
                        format!(
                            "{constraint}: grants are restricted to {} and the vendor's customer id {} isn't among them",
                            if allowed.is_empty() { "no domain".to_string() } else { allowed.join(", ") },
                            id.unwrap_or("(give it with --vendor-customer-id)")
                        ),
                    )
                    .required(why.to_string())
                    .fix(fix("add the vendor's customer id to allowedValues"))
                    .doc("https://cloud.google.com/resource-manager/docs/organization-policy/restricting-domains")
                }
            }
        }
        "iam.disableServiceAccountKeyCreation" => Finding::new(
            "org-policy",
            Outcome::Pass,
            format!(
                "{constraint}: {} ({why})",
                if boolean {
                    "enforced, fine"
                } else {
                    "not enforced"
                }
            ),
        ),
        "iam.disableCrossProjectServiceAccountUsage" => Finding::new(
            "org-policy",
            Outcome::Pass,
            format!(
                "{constraint}: {} ({why})",
                if boolean {
                    "enforced, fine"
                } else {
                    "not enforced"
                }
            ),
        ),
        "compute.vmExternalIpAccess"
        | "compute.requireShieldedVm"
        | "compute.restrictVpcPeering"
        | "compute.skipDefaultNetworkCreation" => {
            let restricts =
                boolean || all == Some("DENY") || (!allowed.is_empty() && all.is_none());
            if t.runtime != Runtime::Gke && constraint != "compute.skipDefaultNetworkCreation" {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: not relevant to Cloud Run"),
                )
            } else if restricts {
                Finding::new(
                    "org-policy",
                    Outcome::Warn,
                    format!("{constraint}: enforced; {why}"),
                )
            } else {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: not enforced"),
                )
            }
        }
        "run.allowedIngress" => {
            if t.runtime != Runtime::CloudRun || unset || all == Some("ALLOW") {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: no restriction that matters"),
                )
            } else if allowed
                .iter()
                .any(|a| *a == "internal" || *a == "internal-and-cloud-load-balancing")
                || all.is_none() && allowed.is_empty() && !denied.contains(&"internal")
            {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: internal ingress is allowed"),
                )
            } else {
                Finding::new(
                    "org-policy",
                    Outcome::Fail,
                    format!(
                        "{constraint}: internal ingress isn't allowed ({})",
                        allowed.join(", ")
                    ),
                )
                .required(why.to_string())
                .fix(fix("allow internal"))
                .doc(doc)
            }
        }
        "gcp.resourceLocations" => {
            if unset || all == Some("ALLOW") {
                Finding::new(
                    "org-policy",
                    Outcome::Pass,
                    format!("{constraint}: any location"),
                )
            } else {
                let region_ok = allowed.iter().any(|a| {
                    let a = a.trim_start_matches("in:");
                    a == t.region
                        || a.strip_suffix("-locations").is_some_and(|group| {
                            t.region.starts_with(group)
                                || group == "us" && t.region.starts_with("us-")
                                || group == "eu" && t.region.starts_with("europe-")
                        })
                }) && !denied
                    .iter()
                    .any(|d| d.trim_start_matches("in:") == t.region);
                if region_ok {
                    Finding::new(
                        "org-policy",
                        Outcome::Pass,
                        format!("{constraint}: {} is allowed", t.region),
                    )
                } else {
                    Finding::new("org-policy", Outcome::Fail, format!("{constraint}: {} isn't in the allowed locations ({})", t.region, allowed.join(", ")))
                        .required(why.to_string())
                        .fix(format!("-var region=<an allowed region>, or {}", fix("allow the region")))
                        .doc("https://cloud.google.com/resource-manager/docs/organization-policy/defining-locations")
                }
            }
        }
        _ => Finding::new(
            "org-policy",
            Outcome::Pass,
            format!(
                "{constraint}: {} ({why})",
                if boolean {
                    "enforced, fine"
                } else {
                    "not enforced"
                }
            ),
        ),
    }
}

/// `testIamPermissions` on the project with an access token from gcloud: the permissions held, or None when it
/// couldn't be asked.
async fn test_permissions(r: &mut ReadOnly<'_>, project: &str) -> Option<Vec<String>> {
    let token = r
        .run(&gcloud(&["auth", "print-access-token"]))
        .ok()
        .filter(|o| o.ok)?
        .stdout
        .trim()
        .to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;
    let url = format!(
        "https://cloudresourcemanager.googleapis.com/v1/projects/{project}:testIamPermissions"
    );
    if !r.quiet {
        eprintln!("  POST {url}");
    }
    let v: Value = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "permissions": TEMPLATE_PERMISSIONS }))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        v["permissions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// A TCP connect within 5 seconds.
fn reach(host_port: &str) -> Result<()> {
    let addr = host_port
        .to_socket_addrs()
        .with_context(|| format!("{host_port} doesn't resolve"))?
        .next()
        .context("no address")?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map(|_| ())
        .with_context(|| format!("connect to {addr}"))
}
