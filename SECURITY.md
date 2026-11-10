# Security policy

AmpUp Memory stores what people and agents remember, so a flaw that lets memory reach someone who must not read
it, or lets deleted memory survive, is the kind of issue we most want to hear about.

## Reporting a vulnerability

**Please don't report security issues in public issues, discussions or pull requests.**

Report it privately through GitHub's private vulnerability reporting: open the repository's
[Security tab](https://github.com/A79-ai/ampup-memory/security) and choose **Report a vulnerability**
([new advisory](https://github.com/A79-ai/ampup-memory/security/advisories/new)). Only the maintainers can see it.

Please include:

- the version or commit you tested (`memstore --version`);
- what an attacker can do, and what they need first (a sign-in, a token, network access to the server);
- the steps or a proof of concept to reproduce it;
- any logs or output that show it.

## What to expect

- We acknowledge a report within 3 business days.
- We tell you whether we accept it, and our plan, within 10 business days.
- We fix accepted issues in a new release, publish a GitHub Security Advisory for it, and credit you there unless
  you'd rather we didn't.

Please give us a reasonable time to release a fix before you disclose the issue publicly; we aim for 90 days at most
and will agree a date with you.

## Supported versions

Security fixes go into the newest release only. Upgrade to it to get them
([releases](https://github.com/A79-ai/ampup-memory/releases)).

## Scope

In scope: the code in this repository (the `memstore` server and CLI, `memstore-client`, the plugins) and the
release binaries built from it. Out of scope: issues in third-party dependencies with no impact on AmpUp Memory
(report those upstream), and deployments you run yourself that change its configuration in ways the documentation
warns against.
