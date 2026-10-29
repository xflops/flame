# Security Policy

## Supported versions

Security fixes are made on `main` and on the latest release series (the
`release-X.Y` branch of the newest `vX.Y.0` release). Older series are not
patched; upgrade to the latest release.

## Reporting a vulnerability

**Please do not report a security vulnerability in a public issue, pull
request or discussion.**

Report it privately through GitHub:
[Report a vulnerability](https://github.com/xflops/flame/security/advisories/new)
(the **Security** tab of the repository, then **Report a vulnerability**).
Only the maintainers can see the report.

Include the version or commit, a description of the vulnerability and its
impact, and the steps to reproduce it (a proof of concept helps). Say whether
you plan to publish your own write-up, so that the disclosure can be agreed.

## What to expect

- We acknowledge a report within **7 days**, and tell you our assessment of it
  (accepted, needs more information, or not a vulnerability) within **14 days**.
- We keep you informed about the progress of the fix.
- We aim to fix a vulnerability of medium or higher severity within **60 days**
  of the report, and sooner for a critical one. A fix is released on `main` and
  on the supported release series, and published with a
  [GitHub security advisory](https://github.com/xflops/flame/security/advisories)
  that names the affected and fixed versions and a CVE identifier.
- The release notes of the release that contains the fix name every fixed
  vulnerability that has a CVE identifier.
- We credit the reporter in the advisory, unless asked not to.

We ask you to give us a reasonable time to fix a vulnerability before you
publish it, and to avoid accessing data that is not yours, degrading a service,
or testing against systems that you do not own.

## Security design and hardening

[docs/security.md](docs/security.md) describes how Flame is designed to be
secure, the kinds of errors that we look for, and how to deploy it safely.
