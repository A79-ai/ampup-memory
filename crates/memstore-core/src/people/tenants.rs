//! Platform tenants: one service (a platform) holds one key and serves many tenants of its own, each request naming
//! one (`x-memstore-tenant`) and, optionally, the platform's user in it (`x-memstore-user`). Each tenant is an org
//! of its own, made on its first request from the platform's template ([`Template`]), and each user a member of it,
//! enrolled on their first request: the platform vouches for them. The platform keeps no state per tenant.
//!
//! - The platform is named by its keys-file row (`"platform": "<name>"`), never by a key id: a key rotated under the
//!   same name keeps its tenants. Its orgs say so (`created_by` is `platform:<name>`).
//! - A tenant maps to its org through the directory ([`Directory::tenants`], recorded with the org's `org.create`),
//!   never by computing the name again: a tenant renamed later keeps its org. The mapping lasts until the org's
//!   memory is purged, so a delete that stopped halfway is finished by the same request repeated.
//! - A user's member name is made from the tenant's org, the platform's user id and how many times that user was
//!   forgotten there ([`member_name`], [`current_member`]): the platform never sees it, and a user forgotten and
//!   named again is a new member, who reads nothing that named the one forgotten. Their email and display name are
//!   attributes ([`super::directory::MemberAttrs`]).
//! - Everything here is planned inside the directory's compare-and-set: two servers racing to make one tenant make
//!   one org, the second planning nothing against the first's commit.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::directory::{
    Change, Directory, Event, Invalid, MemberAttrs, Org, Profile, Refused, Role, Settings,
    TENANTS_VERSION,
};

/// What an org made for a platform's tenant records as its maker: `platform:<name>`.
pub const PLATFORM_MARK: &str = "platform:";

/// Whether `name` may name a platform: lower-case letters, digits and `_`, 1 to 24 of them.
pub fn valid_platform(name: &str) -> bool {
    (1..=24).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What a platform's orgs record as their maker (`Org::created_by`).
pub fn platform_mark(platform: &str) -> String {
    format!("{PLATFORM_MARK}{platform}")
}

/// Whether `id` may be a tenant's or a user's id as a platform names them: 1 to 128 bytes of printable ASCII, compared
/// exactly (case and all).
pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// The id a tenant's own service identity (a platform request naming no user) reads and writes as. Never an ACL id
/// (it holds a `:`), so no entry names it but its tenant's org-wide group, which it holds.
pub fn service_user(platform: &str) -> String {
    platform_mark(platform)
}

/// RFC 4648 base32 of `bytes`, lower-case and unpadded.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// base32 of the sha256 of `a`, a NUL, then `b`.
fn hashed(a: &str, b: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(a.as_bytes());
    h.update([0u8]);
    h.update(b.as_bytes());
    base32(&h.finalize())
}

/// The readable start of a tenant's org name: its name lower-cased, each run of anything but `a-z0-9` one `_`,
/// trimmed of `_`, cut to 24 characters; `t` with no name (or nothing left of it).
pub fn slug(name: Option<&str>) -> String {
    let mut out = String::new();
    for c in name.unwrap_or_default().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let mut out = out.trim_matches('_').to_string();
    out.truncate(24);
    let out = out.trim_end_matches('_');
    match out.is_empty() {
        true => "t".into(),
        false => out.into(),
    }
}

/// The name a new tenant's org is made under: `<slug>_<16 base32 characters of sha256(platform, NUL, tenant)>`, or
/// with `_<n>` after it (2, 3, …) for the first name no org has held: a tenant deleted and named again gets an org
/// of a name never used before, so nothing that wrote to the old one (a server that hadn't seen the delete yet) and
/// no purge of the old one's leftovers can reach the new one. Only ever used to make the org: a tenant is found
/// through the mapping ([`Directory::tenants`]).
pub fn org_name(dir: &Directory, platform: &str, tenant: &str, name: Option<&str>) -> String {
    let base = format!("{}_{}", slug(name), &hashed(platform, tenant)[..16]);
    let held =
        |n: &str| dir.orgs.contains_key(n) || dir.purging.contains(n) || dir.purged.contains(n);
    (1u32..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}_{n}"),
        })
        .find(|n| !held(n))
        .expect("a free name")
}

/// The org name `name` was made under before it, if it has one: `<base>` for `<base>_2`, `<base>_2` for `<base>_3`.
pub fn previous_incarnation(name: &str) -> Option<String> {
    let (base, n) = name.rsplit_once('_')?;
    // A bare `<slug>_<hash>` ends in 16 base32 characters, too long for a u32 even were they all digits.
    let n = n
        .parse::<u32>()
        .ok()
        .filter(|v| *v >= 2 && v.to_string() == n)?;
    match n {
        2 => Some(base.to_string()),
        n => Some(format!("{base}_{}", n - 1)),
    }
}

/// A platform user's member name in their tenant's org at forget generation `generation`: `u<24 base32 characters
/// of sha256(org, NUL, user[, NUL, generation])>@<org>.members.invalid`. An address ACLs can name that no one can
/// sign in as, and that no other tenant's member (nor the same user before a forget) has.
pub fn member_name(org: &str, user: &str, generation: u32) -> String {
    let key = match generation {
        0 => user.to_string(),
        g => format!("{user}\0{g}"),
    };
    format!(
        "u{}@{org}{}",
        &hashed(org, &key)[..24],
        super::directory::MEMBERS_DOMAIN
    )
}

/// A platform user an admin of their tenant removed (not forgot): they aren't enrolled again by being named. Holds
/// the member name they had.
#[derive(Debug)]
pub struct MemberRemoved(pub String);

impl std::fmt::Display for MemberRemoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the user was removed from the tenant: they aren't enrolled again by being named",
        )
    }
}

impl std::error::Error for MemberRemoved {}

/// `user`'s member name in tenant org `org_name` (`org`: as the directory holds it, none if not yet made): the
/// first generation that is a member, or that was never forgotten there. One forgotten (`member.forget`) is skipped:
/// the user named again is a new member. One removed but not forgotten is [`MemberRemoved`].
pub fn current_member(
    org_name: &str,
    org: Option<&Org>,
    user: &str,
) -> std::result::Result<String, MemberRemoved> {
    let mut generation = 0;
    loop {
        let name = member_name(org_name, user, generation);
        let Some(o) = org else { return Ok(name) };
        if o.members.contains_key(&name) {
            return Ok(name);
        }
        if o.forgotten.contains(&name) {
            generation += 1;
            continue;
        }
        if o.removed.contains(&name) {
            return Err(MemberRemoved(name));
        }
        return Ok(name);
    }
}

/// A platform's template for its tenants (`set_tenant_template`): settings, limits and a built-in schema each new
/// tenant starts with. Its settings and limits hold in every tenant of the platform where the tenant hasn't set its
/// own ([`Org::settings`]); its schema is given to a tenant as it is made.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Template {
    #[serde(default, skip_serializing_if = "Settings::is_empty")]
    pub settings: Settings,
    #[serde(default, skip_serializing_if = "crate::limits::OrgLimits::is_empty")]
    pub limits: crate::limits::OrgLimits,
    /// A built-in schema's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

impl Template {
    /// The platform and template a `platform.template` change's detail sets.
    pub fn of_event(detail: &str) -> Option<(String, Template)> {
        let v: Value = serde_json::from_str(detail).ok()?;
        let p = v["platform"].as_str().filter(|p| valid_platform(p))?;
        let t = serde_json::from_value(v["template"].clone()).ok()?;
        Some((p.to_string(), t))
    }
}

/// A tenant whose org is deleted and whose memory isn't purged yet: nothing is served for it until its delete is
/// repeated and finishes.
#[derive(Debug)]
pub struct Purging(pub String);

impl std::fmt::Display for Purging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Purging {}

/// What a platform key may change through the directory's own changes (`directory::plan`): delete an org made for
/// one of its tenants, while the mapping still names it (live, or deleted and not yet purged). Nothing else.
pub fn platform_may(dir: &Directory, platform: &str, change: &Change) -> Result<()> {
    match change {
        Change::DeleteOrg { org, .. }
            if dir
                .tenants
                .iter()
                .any(|((p, _), o)| p == platform && o == org) =>
        {
            Ok(())
        }
        _ => Err(Refused(format!(
            "a platform key changes only its own tenants, through /tenants: not org {}",
            change.org()
        ))
        .into()),
    }
}

/// The user a platform request names, and what it says of them (absent: unchanged).
#[derive(Clone, Debug, Default)]
pub struct User {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// The events that make `user` a member of `org` (`org_name`; none: made in the same commit) and bring their
/// attributes up to what the request says: nothing when the directory already holds that. Also their member name.
/// A user an admin removed is refused ([`MemberRemoved`]).
fn enroll(
    actor: &str,
    org_name: &str,
    org: Option<&Org>,
    user: &User,
) -> Result<(Vec<Event>, String)> {
    let member = current_member(org_name, org, &user.id)?;
    let mut out = vec![];
    if !org.is_some_and(|o| o.members.contains_key(&member)) {
        let mut e = Event::new(actor, "member.add", org_name).user(&member);
        e.role = Some(Role::Member.as_str().into());
        out.push(e);
    }
    let have = org
        .and_then(|o| o.attrs.get(&member))
        .cloned()
        .unwrap_or_default();
    let set = MemberAttrs {
        email: user
            .email
            .clone()
            .filter(|e| have.email.as_ref() != Some(e)),
        name: user.name.clone().filter(|n| have.name.as_ref() != Some(n)),
    };
    if set != MemberAttrs::default() {
        let mut e = Event::new(actor, "member.attrs", org_name)
            .user(&member)
            .detail(&serde_json::to_string(&set).unwrap_or_default());
        e.version = TENANTS_VERSION;
        out.push(e);
    }
    Ok((out, member))
}

/// What [`plan_tenant`] plans: the events (none when the directory holds it all), the tenant's org and the user's
/// member name there.
pub struct Planned {
    pub events: Vec<Event>,
    pub org: String,
    pub member: Option<String>,
}

/// The events that give `platform`'s `tenant` its org (named after `label` if it is made now), with `user` a member
/// of it: nothing if the directory holds all of that already. Planned inside the compare-and-set, so the
/// mapping, not the name, decides whether the tenant has an org. A tenant whose org is being deleted is
/// [`Purging`]; a user an admin removed is [`MemberRemoved`].
pub fn plan_tenant(
    dir: &Directory,
    actor: &str,
    platform: &str,
    tenant: &str,
    label: Option<&str>,
    user: Option<&User>,
) -> Result<Planned> {
    if let Some(org_name) = dir.tenants.get(&(platform.to_string(), tenant.to_string())) {
        let Some(org) = dir.orgs.get(org_name) else {
            return Err(Purging(format!(
                "tenant {tenant:?} is being deleted: repeat DELETE /tenants/{{id}} to finish it, then name it again"
            ))
            .into());
        };
        let (events, member) = match user {
            Some(u) => {
                let (e, m) = enroll(actor, org_name, Some(org), u)?;
                (e, Some(m))
            }
            None => (vec![], None),
        };
        return Ok(Planned {
            events,
            org: org_name.clone(),
            member,
        });
    }
    let name = org_name(dir, platform, tenant, label);
    crate::check_name("org", &name)?;
    let mut create = Event::new(actor, "org.create", &name)
        .detail(&json!({"created_by": platform_mark(platform), "tenant": tenant}).to_string());
    create.version = TENANTS_VERSION;
    let mut events = vec![create];
    if let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) {
        let profile = Profile {
            description: label.to_string(),
            ..Default::default()
        };
        events.push(
            Event::new(actor, "org.profile", &name).detail(&serde_json::to_string(&profile)?),
        );
    }
    let member = match user {
        Some(u) => {
            let (e, m) = enroll(actor, &name, None, u)?;
            events.extend(e);
            Some(m)
        }
        None => None,
    };
    Ok(Planned {
        events,
        org: name,
        member,
    })
}

/// What `set_tenant_template` changes, as `set_settings` and `set_limits` do an org's: each setting and limit
/// given is set over the template's, the others stay; `clear` starts over from an empty template (by a platform's
/// key: what the platform may set, keeping the operator's part).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TemplateChange {
    #[serde(default)]
    pub clear: bool,
    #[serde(default)]
    pub settings: Option<Settings>,
    #[serde(default)]
    pub limits: Option<crate::limits::OrgLimits>,
    /// A built-in schema's name; null takes it away.
    #[serde(default, deserialize_with = "some")]
    pub schema: Option<Option<String>>,
}

fn some<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// The event that sets `platform`'s template as `change` says, by the platform's own key or the operator
/// (`superuser`). A template holds in every tenant of the platform, so it sets nothing that decides who reads what
/// (`reads`) or who signs in (`require_mfa`, `require_sso`). What spends the deployment's budget or keeps more than
/// memory does is the operator's, as it is on any org: limits, models, the training log and advice traces. A
/// platform's `clear` resets only the rest.
pub fn plan_template(
    dir: &Directory,
    actor: &str,
    platform: &str,
    superuser: bool,
    change: &TemplateChange,
) -> Result<Vec<Event>> {
    if !valid_platform(platform) {
        return Err(Invalid(format!(
            "platform {platform:?} is lower-case letters, digits and _, at most 24 characters"
        ))
        .into());
    }
    if !superuser && change.limits.is_some() {
        return Err(Refused(
            "a platform key may not set its tenants' limits: they spend the deployment's budget, so only the operator may"
                .into(),
        )
        .into());
    }
    let held = dir.templates.get(platform).cloned().unwrap_or_default();
    let mut t = match (change.clear, superuser) {
        (false, _) => held,
        (true, true) => Template::default(),
        // The operator's part stays: limits, models, the training log and advice traces.
        (true, false) => Template {
            settings: Settings {
                extract_model: held.settings.extract_model,
                compile_model: held.settings.compile_model,
                aip_model: held.settings.aip_model,
                train_log: held.settings.train_log,
                advice_traces: held.settings.advice_traces,
                ..Default::default()
            },
            limits: held.limits,
            schema: None,
        },
    };
    if let Some(s) = &change.settings {
        if s.reads.is_some() || s.require_mfa.is_some() || s.require_sso.is_some() {
            return Err(Invalid(
                "a tenant template sets no reads, require_mfa or require_sso: they are one org's to set".into(),
            )
            .into());
        }
        if !superuser && (s.sets_models() || s.train_log.is_some() || s.advice_traces.is_some()) {
            return Err(Refused(
                "a platform key may not set its tenants' models, training log or advice traces: only the operator may"
                    .into(),
            )
            .into());
        }
        super::directory::check_values(s)?;
        for m in [&s.extract_model, &s.compile_model, &s.aip_model]
            .into_iter()
            .flatten()
        {
            if !super::directory::valid_model(m) {
                return Err(Invalid(format!("invalid model {m:?}")).into());
            }
        }
        t.settings.merge(s.clone());
    }
    if let Some(l) = &change.limits {
        t.limits.monthly_model_tokens = l.monthly_model_tokens.or(t.limits.monthly_model_tokens);
        t.limits.max_episodes = l.max_episodes.or(t.limits.max_episodes);
        t.limits.expires_at = l.expires_at.or(t.limits.expires_at);
    }
    if let Some(schema) = &change.schema {
        if let Some(name) = schema
            && crate::query::schema::builtin(name).is_none()
        {
            return Err(Invalid(format!("no built-in schema {name}")).into());
        }
        t.schema = schema.clone();
    }
    if dir.templates.get(platform) == Some(&t) {
        return Ok(vec![]);
    }
    let mut e = Event::new(actor, "platform.template", "")
        .detail(&json!({"platform": platform, "template": t}).to_string());
    e.version = TENANTS_VERSION;
    Ok(vec![e])
}

/// Each tenant of `platform`, by tenant id: its org.
pub fn tenants_of<'a>(dir: &'a Directory, platform: &str) -> BTreeMap<&'a str, &'a str> {
    dir.tenants
        .iter()
        .filter(|((p, _), _)| p == platform)
        .map(|((_, t), o)| (t.as_str(), o.as_str()))
        .collect()
}

/// The org that is `platform`'s `tenant`, if it is live.
pub fn tenant_org<'a>(dir: &'a Directory, platform: &str, tenant: &str) -> Option<&'a str> {
    dir.tenants
        .get(&(platform.to_string(), tenant.to_string()))
        .map(String::as_str)
        .filter(|o| dir.orgs.contains_key(*o))
}
