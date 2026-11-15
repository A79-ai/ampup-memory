#!/usr/bin/env python3
"""Wraps built binaries in a wheel for one platform, so `pip install <name>` (or `uv tool install`, `pipx install`)
puts them on PATH.

    harness/wheel.py <binary> <name> <release tag> <platform tag> <out dir> [--with=<binary>...] [--alias=<command>]
        [<license file>...]

The tag is the release's (`client-v0.1.0`, `v0.2.0`, `v0.1.0-rc.13`); the wheel's version is its PEP 440 form
(`0.1.0`, `0.2.0`, `0.1.0rc13`). Each binary goes in the wheel's scripts by its own name: pip, pipx and uv install
them into the environment's bin/ and on PATH as they are (no Python launcher in between). `--with` adds another
binary (the server's wheel is ampup-memory-client with `--with=ampup-memory`). License files (LICENSE, NOTICE,
THIRD-PARTY-NOTICES.txt) go in the wheel's .dist-info/licenses/, which pip installs with the package's metadata.
`--alias` adds the first binary again under a second name (the name it had before a rename: `memstore-client` for
`ampup-memory-client`), a copy, as a wheel can't hold a link and what looks for the old name may want the file
itself. Standard library only: the binaries are already built and checked, nothing is compiled here.
"""

import base64
import hashlib
import os
import re
import sys
import zipfile


def pep440(tag: str) -> str:
    v = re.sub(r"^(client-)?v", "", tag)
    return re.sub(r"-(a|b|rc)\.?(\d+)$", r"\1\2", v)


# The PyPI page of a wheel holding the server (ampup-memory) as well as the client.
SERVER_DESCRIPTION = """\
# AmpUp Memory

The memory for agents that gets better with use: it learns advice from the runs that went well and serves it
before the next one. This package is its command line: `ampup-memory` (the server and CLI) and
`ampup-memory-client` (the sign-in, hooks and MCP bridge coding agents run).

```bash
uv tool install ampup-memory-cli     # or: pipx install ampup-memory-cli, pip install ampup-memory-cli
ampup-memory serve --local           # a memory on this machine, MCP at http://127.0.0.1:8080/mcp
```

Or with nothing installed: `uvx --from ampup-memory-cli ampup-memory serve --local`.
Docs: https://docs.ampup.ai/ampup-memory/quickstart. Source: https://github.com/A79-ai/ampup-memory.
"""


def main() -> None:
    binary, name, tag, platform, out = sys.argv[1:6]
    rest = sys.argv[6:]
    aliases = [a.split("=", 1)[1] for a in rest if a.startswith("--alias=")]
    extra = [a.split("=", 1)[1] for a in rest if a.startswith("--with=")]
    licenses = [a for a in rest if not a.startswith(("--alias=", "--with="))]
    version = pep440(tag)
    dist = f"{name.replace('-', '_')}-{version}"
    binaries = [binary, *extra]
    exes = [os.path.basename(b) for b in binaries]
    if len(set(exes) | set(aliases)) != len(exes) + len(aliases):
        sys.exit(f"wheel.py: two commands by one name in {exes + aliases}")
    if "ampup-memory" in exes:
        summary = "AmpUp Memory, team memory for coding agents: the ampup-memory server and CLI, and ampup-memory-client"
        description = SERVER_DESCRIPTION
    else:
        summary = f"{exes[0]}: the sign-in, hooks and MCP bridge for AmpUp Memory, AmpUp's team memory for coding agents"
        description = ""
    metadata = (
        f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
        f"Summary: {summary}\n"
        "Home-page: https://github.com/A79-ai/ampup-memory\n"
        "Project-URL: Source, https://github.com/A79-ai/ampup-memory\n"
        "License: Apache-2.0\nRequires-Python: >=3.8\n"
    )
    if description:
        metadata += f"Description-Content-Type: text/markdown\n\n{description}"
    wheel = f"Wheel-Version: 1.0\nGenerator: ampup-memory harness/wheel.py\nRoot-Is-Purelib: false\nTag: py3-none-{platform}\n"
    files = [(f"{dist}.data/scripts/{exe}", open(b, "rb").read(), 0o755) for b, exe in zip(binaries, exes)]
    files += [(f"{dist}.data/scripts/{alias}", open(binary, "rb").read(), 0o755) for alias in aliases]
    files += [
        (f"{dist}.dist-info/METADATA", metadata.encode(), 0o644),
        (f"{dist}.dist-info/WHEEL", wheel.encode(), 0o644),
    ]
    files += [(f"{dist}.dist-info/licenses/{os.path.basename(f)}", open(f, "rb").read(), 0o644) for f in licenses]
    record = ""
    for path, data, _ in files:
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        record += f"{path},sha256={digest},{len(data)}\n"
    record += f"{dist}.dist-info/RECORD,,\n"
    files.append((f"{dist}.dist-info/RECORD", record.encode(), 0o644))
    os.makedirs(out, exist_ok=True)
    path = os.path.join(out, f"{dist}-py3-none-{platform}.whl")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for p, data, mode in files:
            info = zipfile.ZipInfo(p, date_time=(1980, 1, 1, 0, 0, 0))
            # A regular file's type bits too: pip marks a script executable only when they say it is one.
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            # The most deflate gives: the server's wheel is ~65 MiB, against PyPI's 100 MiB file limit.
            z.writestr(info, data, compresslevel=9)
    print(path)


if __name__ == "__main__":
    main()
