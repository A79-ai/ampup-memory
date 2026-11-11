"""A thin client for an AmpUp Memory server's HTTP API."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any, TypedDict, cast

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
        timeout: float = 60,
    ) -> None:
        headers = {}
        if token:
            headers["authorization"] = f"Bearer {token}"
        # The wire names keep the engine's original prefix: headers are an interface, not what a person types.
        if org:
            headers["x-memstore-org"] = org
        if team:
            if not org:
                raise ValueError("team needs an org")
            headers["x-memstore-team"] = team
        self._http = httpx.Client(base_url=server.rstrip("/"), headers=headers, timeout=timeout)

    @classmethod
    def from_profile(cls, context: str | None = None, *, timeout: float = 60) -> AmpUpMemory:
        """The server, org, team and sign-in that `ampup-memory-client join` saved for a context."""
        p = credentials.load_profile(context)
        return cls(p.server, p.token, org=p.org, team=p.team, timeout=timeout)

    def close(self) -> None:
        self._http.close()

    def __enter__(self) -> AmpUpMemory:  # noqa: PYI034 (typing.Self needs 3.11)
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def _call(self, method: str, path: str, body: Any = None) -> Any:
        r = self._http.request(method, path, json=body)
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
