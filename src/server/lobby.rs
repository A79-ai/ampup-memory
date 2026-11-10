//! MCP for a person whose client named no org, when the directory doesn't settle on one for them: they belong
//! to several (the client must name one), or to none yet (they may ask to join one). An MCP client that gets a
//! refusal at connect drops the server and its user sees only that the tools are missing (Codex does), so the
//! server answers instead, with what to do: `choose_org` lists their orgs and how to name one; `request_join`
//! asks an org's admins to let them in, as `memstore join` does. Nothing here reads or writes memory.

use std::sync::Arc;

use serde_json::{Value, json};

use super::State;
use crate::Principal;
use crate::people::directory::SignIn;

/// Why the caller is here: the orgs they belong to (several), or none.
pub(super) enum Lobby {
    Several(Vec<String>),
    NoOrg,
}

impl Lobby {
    /// Where the directory doesn't settle the caller's org, the lobby for them; else `None` (one org: the
    /// scope's refusal stands).
    pub(super) fn of(orgs: Vec<String>) -> Option<Lobby> {
        match orgs.len() {
            0 => Some(Lobby::NoOrg),
            1 => None,
            _ => Some(Lobby::Several(orgs)),
        }
    }
}

/// How a client names an org, for `org` (or `<org>` when none is chosen yet).
fn how_to_name(org: &str) -> String {
    let server = std::env::var("AMPUP_MEMORY_PUBLIC_URL")
        .unwrap_or_else(|_| "https://<this AmpUp Memory server>".into());
    let server = server.trim_end_matches('/');
    format!(
        "Name the org in the MCP client's settings for this server: the header `x-memstore-org: {org}` \
        (or `?org={org}` on the server URL), then restart the client.\n\n\
        Codex, in ~/.codex/config.toml:\n\n\
        [mcp_servers.ampup-memory]\n\
        url = \"{server}/mcp\"\n\
        http_headers = {{ \"x-memstore-org\" = \"{org}\" }}\n\n\
        Claude Code: /memstore:login --org {org}"
    )
}

fn instructions(lobby: &Lobby, user: &str) -> String {
    match lobby {
        Lobby::Several(orgs) => format!(
            "AmpUp Memory: {user} belongs to several orgs ({}) and this client named none, so no memory is open yet. \
            Call choose_org to tell the user which orgs are theirs and how to pick one; until they do, \
            AmpUp Memory has nothing to search or remember.",
            orgs.join(", ")
        ),
        Lobby::NoOrg => format!(
            "AmpUp Memory: {user} belongs to no org yet, so no memory is open. If the user names the org they \
            work in, call request_join with it: its admins are emailed to approve. Never guess an org."
        ),
    }
}

fn tools(lobby: &Lobby) -> Value {
    match lobby {
        Lobby::Several(_) => json!([{
            "name": "choose_org",
            "description": "List the AmpUp Memory orgs this user belongs to and how to name one in this client. \
                No memory is open until the client names one.",
            "inputSchema": {"type": "object", "properties": {}},
            // It only reads the caller's own orgs: a client runs it without asking (Codex asks otherwise, and
            // refuses where it can't). request_join mails admins, so it carries no such hint.
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        }]),
        Lobby::NoOrg => json!([{
            "name": "request_join",
            "description": "Ask an AmpUp Memory org's admins to let this user in: they get an email to approve. \
                Only an org the user named; never guess one.",
            "inputSchema": {
                "type": "object",
                "properties": {"org": {"type": "string", "description": "The org's name, as the user gave it"}},
                "required": ["org"],
            },
        }]),
    }
}

/// The JSON-RPC answer to `body`, or `None` for a notification.
pub(super) async fn answer(
    state: &Arc<State>,
    who: &Principal,
    via: &SignIn,
    lobby: &Lobby,
    body: &[u8],
) -> Option<Value> {
    let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let id = req.get("id").cloned()?;
    let ok = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let text = |t: String, is_error: bool| json!({"content": [{"type": "text", "text": t}], "isError": is_error});
    match req["method"].as_str().unwrap_or_default() {
        "initialize" => ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": crate::naming::MCP_SERVER, "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions(lobby, who.user()),
        })),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools(lobby)})),
        "tools/call" => {
            let args = &req["params"]["arguments"];
            match (req["params"]["name"].as_str().unwrap_or_default(), lobby) {
                ("choose_org", Lobby::Several(orgs)) => ok(text(
                    format!(
                        "{} belongs to these AmpUp Memory orgs: {}.\n\n{}",
                        who.user(),
                        orgs.join(", "),
                        how_to_name(orgs.first().map_or("<org>", |o| o.as_str()))
                    ),
                    false,
                )),
                ("request_join", Lobby::NoOrg) => {
                    let org = args["org"].as_str().unwrap_or_default().trim();
                    if org.is_empty() {
                        return ok(text("name the org to ask to join".into(), true));
                    }
                    let change = json!({"op": "request_join", "org": org}).to_string();
                    match super::directory_change(state, who, via, false, change.as_bytes()).await {
                        // Approved, it is their one org: the client needs no naming, only a restart.
                        Ok(_) => ok(text(
                            format!(
                                "Asked to join {org}: its admins are emailed to approve. Once one does, restart \
                                this client and memory opens in {org}."
                            ),
                            false,
                        )),
                        // A refusal says why; anything else (the store failing) says only that it failed.
                        Err(e) => match e.downcast_ref::<crate::people::directory::Refused>() {
                            Some(r) => ok(text(format!("not asked: {}", r.0), true)),
                            None => {
                                eprintln!(
                                    "lobby: request_join by {} to {org} failed: {e:#}",
                                    who.user()
                                );
                                ok(text(
                                    "not asked: the request failed, try again".into(),
                                    true,
                                ))
                            }
                        },
                    }
                }
                (name, _) => ok(text(
                    format!("no tool {name} here: {}", instructions(lobby, who.user())),
                    true,
                )),
            }
        }
        m => Some(json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {m}")}})),
    }
}
