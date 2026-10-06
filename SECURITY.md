# Security policy

PackSync handles two things that deserve care: **a Google refresh token** and **people's worlds**. Please
report anything that could expose the first or corrupt/destroy the second.

## Reporting a vulnerability

**Do not open a public issue with exploit details.**

This project has not been published yet, so there is no hosted reporting channel and no contact address that
this file could honestly name. Whoever publishes the repository must, as part of publishing it:

1. enable GitHub's *private vulnerability reporting* (Security → Advisories → "Report a vulnerability"), and
2. add a contact address for the maintainers to this file.

Until then, report to the person who gave you this copy. When reporting, include the PackSync version, your
operating system, what you did, and what you saw; a redacted `packsync doctor --out report.txt` helps.

## What counts

Examples of things we want to hear about:

* the refresh token, an access token, an authorization code, an OAuth client secret or a resumable-upload URL
  appearing anywhere it should not (a log, an error, `--json` output, the cloud, a snapshot, a diagnostics report);
* a way for cloud content (a manifest, an index, a pack, a path) to write, read or delete **outside** the
  instance folder, follow a symlink, exhaust memory or disk, or crash the app;
* a way to make PackSync delete or overwrite data it was not asked to (including cleanup removing objects it
  did not create, or "missing means deleted");
* credentials or secrets ending up in a snapshot despite the sealed rules;
* the development bridge being reachable by something other than the local test harness, or without its token;
* launcher-hook command injection (quoting, instance names, paths);
* a downgrade of a documented guarantee (`docs/consistency.md`, `docs/threat-model.md`).

Out of scope (documented limits, not vulnerabilities): the cloud data is not end-to-end encrypted in v1;
a malicious *mod* in a valid snapshot; a compromised operating system or administrator; sharing one instance
between several people.

## What to expect

There is no service-level promise for a pre-release project run by volunteers, but a report with a way to
reproduce it will be taken seriously, acknowledged, fixed with a regression test, and credited if you wish.

## Supported versions

Only the latest commit of the default branch.
