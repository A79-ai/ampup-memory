#!/usr/bin/env python3
"""Whether a release's wheels go to PyPI: prints `publish=true` or `publish=false` (for $GITHUB_OUTPUT), and why on
stderr, as a workflow annotation when it is a skip worth seeing. The release workflows run it before uploading.

    .github/scripts/pypi-publishable.py <project> <release tag> <wheel>...

Skipped, as PyPI never takes a version twice and keeps every file it took:
- a pre-release (`v0.1.0-rc.46`): pip and uv pass it over for a final release, so it would only use up space;
- a version below the newest final one on PyPI: nobody would get it. This is how `ampup-memory-cli` passes from the
  client alone (0.1.x, client-v* releases) to the server and client together (server releases from v0.2.0): once
  a server release is up, client releases skip;
- a version below PYPI_FROM, when set: the server's releases set it to 0.2.0, as 0.1.x on PyPI is the client alone
  and its versions are the client's (a server v0.1.20 would be client-v0.1.20's number);
- PYPI_MIN_DAYS (default 0) set, and the newest final release on PyPI is younger than that many days: a server
  release's wheels are ~100 MiB each, and the project's total is capped (10 GiB unless PyPI raises it);
- a wheel over PYPI_FILE_LIMIT_MIB (default 100, PyPI's per-file limit unless it raises the project's): none of
  the release's wheels go, so no version is ever up for some platforms only (pip on the others would install an
  older one without saying so).

The same version as PyPI's newest isn't skipped: a run again uploads what the first didn't (the upload skips files
already there). Standard library only.
"""

import datetime
import json
import os
import re
import sys
import urllib.error
import urllib.request

PEP440 = re.compile(r"^(\d+(?:\.\d+)*)(?:(a|b|rc)(\d+))?$")


def pep440(tag: str) -> str:
    v = re.sub(r"^(client-)?v", "", tag)
    return re.sub(r"-(a|b|rc)\.?(\d+)$", r"\1\2", v)


def key(version: str):
    m = PEP440.match(version)
    if not m:
        return None
    release = tuple(int(p) for p in m.group(1).split("."))
    release += (0,) * (4 - len(release))
    pre = (("a", "b", "rc").index(m.group(2)), int(m.group(3))) if m.group(2) else (3, 0)
    return release + pre


def skip(why: str, annotate: bool = False) -> None:
    print(f"::warning::{why}" if annotate else why, file=sys.stderr)
    print("publish=false")
    sys.exit(0)


def main() -> None:
    project, tag, *wheels = sys.argv[1:]
    if not wheels:
        sys.exit("usage: pypi-publishable.py <project> <release tag> <wheel>...")
    version = pep440(tag)
    ours = key(version)
    if ours is None:
        sys.exit(f"{tag}: {version} isn't a version this script reads")
    if ours[4] != 3:
        skip(f"{project} {version}: a pre-release, not published to PyPI")
    floor = os.environ.get("PYPI_FROM")
    if floor and ours < key(floor):
        skip(f"{project} {version}: below PYPI_FROM={floor}; not published")

    try:
        with urllib.request.urlopen(f"https://pypi.org/pypi/{project}/json", timeout=30) as r:
            releases = json.load(r)["releases"]
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        releases = {}  # a project's first upload
    finals = [(key(v), v, files) for v, files in releases.items() if key(v) and key(v)[4] == 3 and files]
    if finals:
        _, newest, files = max(finals)
        if ours < key(newest):
            skip(f"{project} {version}: below {newest}, the newest on PyPI; not published")
        days = float(os.environ.get("PYPI_MIN_DAYS") or 0)
        uploaded = max(datetime.datetime.fromisoformat(f["upload_time_iso_8601"].replace("Z", "+00:00")) for f in files)
        age = datetime.datetime.now(datetime.timezone.utc) - uploaded
        if ours != key(newest) and age < datetime.timedelta(days=days):
            skip(f"{project} {version}: {newest} went up {age.days} days ago, under PYPI_MIN_DAYS={days:g}; not published")

    limit = float(os.environ.get("PYPI_FILE_LIMIT_MIB") or 100)
    big = [(os.path.basename(w), os.path.getsize(w) / 2**20) for w in wheels if os.path.getsize(w) > limit * 2**20]
    if big:
        sizes = ", ".join(f"{n} {s:.1f} MiB" for n, s in big)
        skip(f"{project} {version}: over PyPI's {limit:g} MiB file limit ({sizes}); no wheel published", annotate=True)

    print(f"{project} {version}: publishing {len(wheels)} wheels", file=sys.stderr)
    print("publish=true")


if __name__ == "__main__":
    main()
