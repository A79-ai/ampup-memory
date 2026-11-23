"""A thin client for an AmpUp Memory server's HTTP API."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any, Literal, TypedDict, cast
from urllib.parse import quote

import httpx

from ampup_memory import credentials

INGEST_BATCH = 500


class _EpisodeRequired(TypedDict):
    source: str
    body: str
    occurred_at: int  # microseconds since the epoch, see micros()
    acl: list[str]  # at least one reader, never "*"


class Episode(_EpisodeRequired, total=False):
    about: list[str]
    index_text: bool
    document: str


class Hit(TypedDict, total=False):
    entity: str
    predicate: str
    value: Any
    valid_from: str | None
    valid_to: str | None
    fact_id: str
    source_episodes: list[str]
    quote: str | None
    score: float
    written_by: str | None


class Advice(TypedDict, total=False):
    """What the server served: the situation it matched and its advice, both None when it has none yet."""

    situation: str | None
    advice: str | None
    evidence: Any  # how the advice has fared, when it was learned from runs
    skills: list[Any]
    own_situation: str  # your own note, served beside the team's
    advice_pending: str  # still being found: ask again at the next step
    session_id: str  # on a session's opening


Moment = Literal["next", "fork", "stuck", "done"]


class AmpUpMemoryError(Exception):
    def __init__(self, status: int, message: str, retry_after: float | None = None) -> None:
        super().__init__(f"HTTP {status}: {message}")
        self.status = status
        self.message = message
        self.retry_after = retry_after


def micros(dt: datetime) -> int:
    """`occurred_at` for an episode: microseconds since the epoch (naive datetimes are taken as local)."""
    return (dt.astimezone() - _EPOCH) // timedelta(microseconds=1)  # exact: no float on the way


_EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)


class AmpUpMemory:
    def __init__(
        self,
        server: str,
        token: str | None,
        *,
        org: str | None = None,
        team: str | None = None,
        tenant: str | None = None,
        tenant_name: str | None = None,
        user: str | None = None,
        user_email: str | None = None,
        user_name: str | None = None,
        timeout: float = 60,
    ) -> None:
        """A client for `server`. A platform key names a `tenant` (its own id for one of its customers) and,
        optionally, the `user` acting in it; see `for_tenant` and `for_user`."""
        headers = {}
        if token:
            headers["authorization"] = f"Bearer {token}"
        self._http = httpx.Client(base_url=server.rstrip("/"), headers=headers, timeout=timeout)
        self._owns_http = True
        self._headers = _identity(org, team, tenant, tenant_name, user, user_email, user_name)
        self._tenant, self._tenant_name = tenant, tenant_name
        self.tenants = Tenants(self)

    @classmethod
    def from_profile(cls, context: str | None = None, *, timeout: float = 60) -> AmpUpMemory:
        """The server, org, team and sign-in that `ampup-memory-client join` saved for a context."""
        p = credentials.load_profile(context)
        return cls(p.server, p.token, org=p.org, team=p.team, timeout=timeout)

    def for_tenant(self, tenant: str, *, name: str | None = None) -> AmpUpMemory:
        """A copy of this client for another tenant, as the tenant itself (no user). `name` labels the tenant
        when this request creates it. The copy shares this client's connections: close this one, not the copy."""
        return self._copy(_identity(None, None, tenant, name, None, None, None), tenant, name)

    def for_user(self, user: str, *, email: str | None = None, name: str | None = None) -> AmpUpMemory:
        """A copy of this client for another user of the same tenant. `email` and `name` describe the user (they
        are stored and kept up to date, never the user's identity). The copy shares this client's connections."""
        if self._tenant is None:
            raise ValueError("for_user needs a client with a tenant")
        headers = _identity(None, None, self._tenant, self._tenant_name, user, email, name)
        return self._copy(headers, self._tenant, self._tenant_name)

    def _copy(self, headers: dict[str, str], tenant: str, tenant_name: str | None) -> AmpUpMemory:
        c = object.__new__(AmpUpMemory)
        c._http = self._http
        c._owns_http = False
        c._headers = headers
        c._tenant, c._tenant_name = tenant, tenant_name
        c.tenants = Tenants(c)
        return c

    def close(self) -> None:
        """Close the connections; a copy from `for_tenant` or `for_user` leaves them to the client it came from."""
        if self._owns_http:
            self._http.close()

    def __enter__(self) -> AmpUpMemory:  # noqa: PYI034 (typing.Self needs 3.11)
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def _call(
        self, method: str, path: str, body: Any = None, *, params: dict[str, Any] | None = None, scoped: bool = True
    ) -> Any:
        """One request. `scoped=False` leaves out the org, team, tenant and user headers."""
        r = self._http.request(method, path, json=body, params=params, headers=self._headers if scoped else None)
        try:
            data = r.json()
        except ValueError:
            data = None
        if r.is_error:
            msg = data.get("error") if isinstance(data, dict) else None
            retry = (data.get("retry_after_secs") if isinstance(data, dict) else None) or r.headers.get("retry-after")
            raise AmpUpMemoryError(r.status_code, msg or r.text or r.reason_phrase, float(retry) if retry else None)
        return data

    def _row(self, path: str, *args: Any) -> Any:
        """The row protocol: send one row `[0, *args]`, get back that row's value."""
        out = self._call("POST", path, {"data": [[0, *args]]})["data"][0][1]
        if isinstance(out, dict) and set(out) == {"error"}:
            raise AmpUpMemoryError(200, out["error"])
        return out

    def me(self) -> dict[str, Any]:
        """Who you are: user, groups, scope and memberships."""
        return cast(dict[str, Any], self._call("GET", "/me"))

    def ingest(self, episodes: list[Episode]) -> list[str]:
        """Commit episodes in batches of 500; the ids of all of them. Needs an ingest or admin key."""
        ids: list[str] = []
        for i in range(0, len(episodes), INGEST_BATCH):
            ids += self._call("POST", "/ingest", {"episodes": episodes[i : i + INGEST_BATCH]})["episode_ids"]
        return ids

    def remember(
        self, source: str, body: str, *, acl: list[str] | None = None, document: str | None = None
    ) -> dict[str, Any]:
        """Add a note: `{episode_id, scope, readers}`, or `{queued, message}` when the server queues it."""
        req: dict[str, Any] = {"source": source, "body": body}
        if acl is not None:
            req["acl"] = acl
        if document is not None:
            req["document"] = document
        return cast(dict[str, Any], self._call("POST", "/remember", req))

    def forget(self, episode_id: str) -> str:
        """Forget an episode you wrote; its id."""
        return cast(str, self._call("POST", "/forget", {"episode_id": episode_id})["forgotten"])

    def search(self, query: str, k: int = 10) -> list[Hit]:
        """The facts that best match `query`, with the episodes they came from."""
        return cast(list[Hit], self._row("/search", query, k))

    def query(self, q: dict[str, Any]) -> dict[str, Any]:
        """A query-language request, e.g. {"find": "passages", "text": "renewal"}."""
        return cast(dict[str, Any], self._row("/query", q))

    def episode(self, episode_id: str) -> str | None:
        """An episode's text, or None if it doesn't exist or you can't read it."""
        return cast("str | None", self._row("/episode", episode_id))

    # The learning loop: a session per task, advice as it goes, and how it went at the end. Advice is learned from
    # sessions that ended with a reward.

    def session(self, task: str, *, harness: str | None = None) -> Session:
        """Start a session for `task`; its opening advice is `session.opening`.

        `harness` names what runs the session (your app, say). A name ending in ":sdk" marks the session a
        program's: advice isn't learned from it, nor served to it unless the org turns that on. A platform's agent
        sessions use their own harness name, never one ending in ":sdk".
        """
        req: dict[str, Any] = {"context": task}
        if harness is not None:
            req["harness"] = harness
        opening = cast(Advice, self._call("POST", "/sessions", req))
        return Session(self, opening["session_id"], task, opening)

    def advice(self, question: str) -> Advice:
        """Advice for `question` outside any session. Needs a server with a routing model (503 without one)."""
        return cast(Advice, self._call("POST", "/advice", {"question": question}))

    def reward(self, run_id: str, reward: float, reason: str | None = None) -> str:
        """Reward a session or run you recorded, from -1 to 1, after it ended (say, once its change merged).

        An ended session can be rewarded once the server has written it, within a minute of its end by default
        (404 before then). To reward it as it ends, pass `reward` to `Session.end`."""
        req: dict[str, Any] = {"run_id": run_id, "reward": reward}
        if reason is not None:
            req["reason"] = reason
        return cast(str, self._call("POST", "/runs/reward", req)["run_id"])


def _identity(
    org: str | None,
    team: str | None,
    tenant: str | None,
    tenant_name: str | None,
    user: str | None,
    user_email: str | None,
    user_name: str | None,
) -> dict[str, str]:
    """The headers that say which org or tenant, and which user, a request is for."""
    if team and not org:
        raise ValueError("team needs an org")
    if tenant is not None and org:
        raise ValueError("pass a tenant or an org, not both: a platform key reaches its tenants by their own ids")
    if tenant is None and (tenant_name or user is not None):
        raise ValueError("tenant_name and user need a tenant")
    if user is None and (user_email or user_name):
        raise ValueError("user_email and user_name need a user")
    # The wire names keep the engine's original prefix: headers are an interface, not what a person types.
    h: dict[str, str] = {}
    if org:
        h["x-memstore-org"] = org
    if team:
        h["x-memstore-team"] = team
    # Ids go as they are, checked; the tenant's name and the user's email and name are attributes, sent as
    # percent-encoded UTF-8 (RFC 3986, everything outside the unreserved set encoded) so any text travels.
    for header, param, value, is_id in (
        ("x-memstore-tenant", "tenant", tenant, True),
        ("x-memstore-tenant-name", "tenant_name", tenant_name, False),
        ("x-memstore-user", "user", user, True),
        ("x-memstore-user-email", "user_email", user_email, False),
        ("x-memstore-user-name", "user_name", user_name, False),
    ):
        if is_id and value is not None:
            h[header] = _id(param, value)
        elif value:
            h[header] = quote(value, safe="")
    return h


def _id(param: str, value: str) -> str:
    """A tenant or user id: 1 to 128 printable ASCII characters."""
    if not (1 <= len(value) <= 128 and all(" " <= c <= "~" for c in value)):
        raise ValueError(f"{param} must be 1 to 128 printable ASCII characters: {value!r}")
    return value


class Tenants:
    """A platform key's tenants: `AmpUpMemory.tenants`. Each call names its tenant in the path, so it sends no
    tenant or user headers, whatever the client was made for."""

    def __init__(self, memory: AmpUpMemory) -> None:
        self._memory = memory

    def ensure(self, tenant: str, name: str | None = None) -> dict[str, Any]:
        """Create the tenant if it doesn't exist: `{tenant, org, created}`. `name` labels it when it is created."""
        body = {} if name is None else {"name": name}
        return cast(dict[str, Any], self._memory._call("PUT", _tenant_path(tenant), body, scoped=False))

    def get(self, tenant: str) -> dict[str, Any]:
        """`{tenant, org, created_at, limits, usage}`; raises a 404 for a tenant that doesn't exist."""
        return cast(dict[str, Any], self._memory._call("GET", _tenant_path(tenant), scoped=False))

    def list(self, after: str | None = None, limit: int | None = None) -> dict[str, Any]:
        """A page of the platform's tenants with their usage, after the tenant id `after` (up to 500 a page)."""
        params = {k: v for k, v in (("after", after), ("limit", limit)) if v is not None}
        return cast(dict[str, Any], self._memory._call("GET", "/tenants", params=params or None, scoped=False))

    def delete(self, tenant: str) -> dict[str, Any]:
        """Purge the tenant and everything in it. `{purging: true}` while the purge runs: call again to finish it.
        The same id later starts a new, empty tenant."""
        return cast(dict[str, Any], self._memory._call("DELETE", _tenant_path(tenant), scoped=False) or {})

    def forget_user(self, tenant: str, user: str) -> dict[str, Any]:
        """Forget one of the tenant's users: take them out of it and delete what they wrote there."""
        path = f"{_tenant_path(tenant)}/users/{quote(_id('user', user), safe='')}/forget"
        return cast(dict[str, Any], self._memory._call("POST", path, scoped=False) or {})


def _tenant_path(tenant: str) -> str:
    return f"/tenants/{quote(_id('tenant', tenant), safe='')}"


class Session:
    """One task's session: from `AmpUpMemory.session`."""

    def __init__(self, memory: AmpUpMemory, session_id: str, task: str, opening: Advice) -> None:
        self._memory = memory
        self.id = session_id
        self.task = task
        self.opening = opening

    def advice(
        self, question: str | None = None, *, moment: Moment | None = None, conversation: str | None = None
    ) -> Advice:
        """Advice for this session, recorded in it. `question` is the task (the default) or the step at hand;
        `moment` says where you are: "next" (moving to the next step), "fork" (choosing an approach), "stuck" or
        "done". `conversation` is a condensed account of the session so far."""
        req: dict[str, Any] = {"session_id": self.id, "question": question or self.task}
        if moment is not None:
            req["moment"] = moment
        if conversation is not None:
            req["conversation_so_far"] = conversation
        return cast(Advice, self._memory._call("POST", "/advice", req))

    def end(
        self,
        outcome: str | None = None,
        reward: float | None = None,
        *,
        transcript: str | None = None,
        reason: str | None = None,
    ) -> dict[str, Any]:
        """End the session: what came of it, and a reward from -1 (failed) to 1 (succeeded) if you know it now
        (else reward it later with `AmpUpMemory.reward(session.id, ...)`). `transcript` is what happened."""
        req: dict[str, Any] = {"session_id": self.id, "task": self.task}
        for k, v in (("outcome", outcome), ("reward", reward), ("transcript", transcript), ("reason", reason)):
            if v is not None:
                req[k] = v
        return cast(dict[str, Any], self._memory._call("POST", "/sessions/end", req))
