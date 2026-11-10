#!/usr/bin/env python3
"""Checks that every permission named in deploy/iam exists, which `terraform validate` can't do.

AWS: each "service:Action" in deploy/iam/aws is looked up in AWS's published service reference
(https://servicereference.us-east-1.amazonaws.com, no credentials needed).

GCP: each permission in a custom role under deploy/iam/gcp is looked up in
`gcloud iam list-testable-permissions` for a project you can read, and must be usable in a custom role
(customRolesSupportLevel SUPPORTED, or TESTING with a note). Skipped without --gcp-project.
Read-only: nothing is created.

    deploy/iam/check_permissions.py [--gcp-project <project>]
"""

import argparse
import json
import pathlib
import re
import subprocess
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent
AWS_REFERENCE = "https://servicereference.us-east-1.amazonaws.com/v1/{0}/{0}.json"


def strings(cloud):
    for path in sorted((ROOT / cloud).rglob("*.tf")):
        if ".terraform" in path.parts:
            continue
        for match in re.finditer(r'"([^"\s$]+)"', path.read_text()):
            yield path.relative_to(ROOT), match.group(1)


def check_aws():
    actions = {}
    for path, s in strings("aws"):
        m = re.fullmatch(r"([a-z0-9-]+):([A-Za-z0-9]+)", s)
        if m and m.group(1) not in ("aws", "arn"):
            actions.setdefault(s, path)
    known, keys, bad, checked = {}, set(), [], 0
    for action, path in sorted(actions.items()):
        service, name = action.split(":")
        if service not in known:
            with urllib.request.urlopen(AWS_REFERENCE.format(service), timeout=30) as r:
                ref = json.load(r)
            known[service] = {a["Name"] for a in ref["Actions"]}
            keys |= {k["Name"] for k in ref.get("ConditionKeys", [])}
        if action in keys:
            continue  # a condition key such as iam:PassedToService
        checked += 1
        if name not in known[service]:
            bad.append(f"{path}: {action} is not an AWS action")
    print(f"aws: {checked} actions checked")
    return bad


def check_gcp(project):
    perms = {}
    for path, s in strings("gcp"):
        if re.fullmatch(r"[a-z]+\.[a-zA-Z]+\.[a-zA-Z]+", s) and not s.endswith((".com", ".goog")):
            perms.setdefault(s, path)
    run = subprocess.run(
        ["gcloud", "iam", "list-testable-permissions",
         f"//cloudresourcemanager.googleapis.com/projects/{project}", "--format=json"],
        capture_output=True, text=True,
    )
    if run.returncode != 0:
        return [f"gcp: gcloud iam list-testable-permissions failed (log in with gcloud auth login?): "
                f"{run.stderr.strip().splitlines()[0] if run.stderr.strip() else run.returncode}"]
    out = run.stdout
    support = {p["name"]: p.get("customRolesSupportLevel", "SUPPORTED") for p in json.loads(out)}
    bad = []
    for perm, path in sorted(perms.items()):
        level = support.get(perm)
        if level is None:
            bad.append(f"{path}: {perm} is not a permission")
        elif level == "NOT_SUPPORTED":
            bad.append(f"{path}: {perm} can't be used in a custom role")
        elif level != "SUPPORTED":
            print(f"note: {path}: {perm} is {level} in custom roles (allowed; see docs/privileges.md)")
    print(f"gcp: {len(perms)} permissions checked")
    return bad


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--gcp-project", help="a project to list testable permissions on (read-only)")
    args = parser.parse_args()
    bad = check_aws()
    if args.gcp_project:
        bad += check_gcp(args.gcp_project)
    else:
        print("gcp: skipped (no --gcp-project)")
    for line in bad:
        print(line, file=sys.stderr)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
