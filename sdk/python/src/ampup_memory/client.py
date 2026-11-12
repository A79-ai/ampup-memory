"""A thin client for an AmpUp Memory server's HTTP API."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any, Literal, TypedDict, cast

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

    # The learning loop: a session per task, advice as it goes, and how it went at the end. Advice is learned from
    # sessions that ended with a reward.

    def session(self, task: str, *, harness: str | None = None) -> Session:
        """Start a session for `task`; its opening advice is `session.opening`.

        `harness` names what runs the session (your app, say). A name ending in ":sdk" marks the session a
        program's: advice isn't learned from it, nor served to it unless the org turns that on.
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
