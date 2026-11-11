"""The context and sign-in that `ampup-memory-client join` saved, renewed when it expires.

They live where the CLI keeps them: `AMPUP_MEMORY_CONFIG_DIR` (or the older `MEMSTORE_CONFIG_DIR`), else
`~/.ampup-memory`, else `~/.memstore` on a machine where the CLI hasn't moved it yet.
"""

from __future__ import annotations

import base64
import json
import os
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

SIGN_IN = "`ampup-memory-client join <server> --org <org>`"


@dataclass
class Profile:
    server: str
    token: str
    org: str | None = None
    team: str | None = None


def _env(name: str) -> str | None:
    """`AMPUP_MEMORY_<name>`, else the older `MEMSTORE_<name>` (the new name wins, as in the CLI)."""
    return os.environ.get(f"AMPUP_MEMORY_{name}") or os.environ.get(f"MEMSTORE_{name}") or None


def config_dir() -> Path:
    d = _env("CONFIG_DIR")
    if d:
        return Path(d)
    new, old = Path.home() / ".ampup-memory", Path.home() / ".memstore"
    if not os.path.lexists(new) and old.is_dir():
        return old
    return new


def _read(name: str) -> dict[str, Any]:
    p = config_dir() / name
    return json.loads(p.read_text()) if p.exists() else {}


def _write_private(name: str, data: dict[str, Any]) -> None:
    d = config_dir()
    d.mkdir(parents=True, exist_ok=True)
    tmp = d / f".{name}.{os.getpid()}.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "w") as f:
            f.write(json.dumps(data, indent=2))
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, d / name)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


@contextmanager
def _renewal_lock() -> Iterator[None]:
    """The CLI's lock on `credentials.lock`, held while renewing: its hooks and MCP server renew the same sign-in,
    and a refresh token used twice (where the provider rotates them) can sign the person out everywhere."""
    d = config_dir()
    d.mkdir(parents=True, exist_ok=True)
    fd = os.open(d / "credentials.lock", os.O_WRONLY | os.O_CREAT, 0o600)
    try:
        try:
            import fcntl
        except ImportError:  # Windows: no flock, renew unlocked
            yield
            return
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def load_profile(context: str | None = None) -> Profile:
    config = _read("config.json")
    name = context or _env("CONTEXT") or config.get("current-context")
    ctx = config.get("contexts", {}).get(name) if name else None
    if not ctx or not ctx.get("server"):
        raise RuntimeError(f"no context {name!r} with a server in {config_dir()}: run {SIGN_IN}")
    server = ctx["server"].rstrip("/")
    org = ctx.get("org")
    all_creds = _read("credentials.json")
    key = f"{server}#{org}" if org and f"{server}#{org}" in all_creds else server
    if key not in all_creds:
        raise RuntimeError(f"not signed in to {server}: run {SIGN_IN}")
    return Profile(server, bearer(key), org, ctx.get("team"))


def bearer(key: str) -> str:
    """The API key, else the ID (or access) token, renewed first if it expires within a minute."""
    creds = _read("credentials.json")[key]
    if creds.get("api_key"):
        return str(creds["api_key"])
    if creds.get("expires_at", 0) > time.time() + 60:
        return _current(creds)
    with _renewal_lock():
        # Another process may have renewed while this one waited: take its sign-in, so its refresh token is newest.
        all_creds = _read("credentials.json")
        creds = all_creds[key]
        if creds.get("expires_at", 0) <= time.time() + 60:
            if not creds.get("refresh_token"):
                raise RuntimeError(f"signed-in token expired: run {SIGN_IN}")
            form = {
                "grant_type": "refresh_token",
                "refresh_token": creds["refresh_token"],
                "client_id": creds.get("client_id", ""),
            }
            if creds.get("client_secret"):
                form["client_secret"] = creds["client_secret"]
            r = httpx.post(creds["token_endpoint"], data=form, headers={"accept": "application/json"}, timeout=30)
            if r.is_error:
                raise RuntimeError(f"renewing the sign-in failed ({r.status_code}): run {SIGN_IN}")
            t = r.json()
            creds["access_token"] = t["access_token"]
            for k in ("id_token", "refresh_token"):
                if t.get(k):
                    creds[k] = t[k]
            creds["expires_at"] = _jwt_exp(creds.get("id_token")) or int(time.time()) + int(t.get("expires_in", 3600))
            all_creds[key] = creds
            _write_private("credentials.json", all_creds)
    return _current(creds)


def _current(creds: dict[str, Any]) -> str:
    return str(creds.get("id_token") or creds["access_token"])


def _jwt_exp(jwt: str | None) -> int | None:
    if not jwt or jwt.count(".") != 2:
        return None
    payload = jwt.split(".")[1]
    try:
        return int(json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))["exp"])
    except (ValueError, KeyError, TypeError):
        return None
