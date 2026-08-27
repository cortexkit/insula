# insula public-flip precheck — whole-history exposure audit

Run 2026-08-27 against the runbook at
`subconscious/docs/audits/public-history-rewrite-runbook.md`. The runbook's rule
is that the audit DECIDES whether a history rewrite is needed; nothing
destructive happens before this inventory lands.

**Verdict: NO history rewrite. Tip cleanup only.** Evidence below.

## Scale

```
commits                767          files at tip       118
blobs in object store  1701         deleted-ever paths   1
refs                     8          forks                0
remote heads             1          releases             0
tags                     0          PRs ever             1
Actions runs           644          issues+PRs          13
```

## Verifier

Narrow classes only, per the rehearsal lesson that a generic ip:port class ate
loopback examples in surviving files. Classes: `openai-key`, `anthropic-key`,
`alibaba-key`, `github-token`, `slack-token`, `aws-key-id`, `google-api-key`,
`jwt`, `private-key-pem`, `email` (excluding example/test/localhost domains),
`cookie-session` (three named cookie families this module reads).

Contains no banned literals. Proved in **both** directions before any verdict was
trusted: six synthetic positives fire on the right class, five must-not-fire
controls (`user@example.com`, `test@test.localhost`, `http://127.0.0.1:8477/v1`,
`sk-short`, `not-a-jwt.value`) stay silent.

## Coverage, stated as a denominator

```
reachable text blobs      1623
unreachable blobs           78   scanned too, see below
                          ----
total blobs in store      1701   100%
commit messages            767   100%
issue/PR texts             136   across all 13 issues+PRs
```

The 78 were the gap between `rev-list --objects --all` (reachable) and
`cat-file --batch-all-objects` (everything on disk) — amended commits and
mutation-probe restores. They never travel on push or clone, and they were
scanned anyway because a clean result over a subset reads as coverage and is
worse than no result.

## Findings

**Credential classes: zero hits.** No key, token, JWT or private key has ever
been committed to this repository, in any blob or commit message, reachable or
not.

**Email class: 3 distinct hits, all synthetic test vectors.** Read individually:

| specimen | site | what it is |
| --- | --- | --- |
| `User@Example.COM` | `codex.rs` test | mixed-case claim fixture for the email-extraction test |
| `user:pass@api.example.com` | `sub2api.rs` test | userinfo URL the validator must REJECT |
| `user:pass@api.neuralwatt.com` | `neuralwatt.rs` test | same, provider-hostname variant |

The last two are the fixtures for the loopback/userinfo guards — they exist to
prove credentials in a URL are refused. Removing them would delete the tests
that defend against the exposure class this audit is looking for.

**Cookie/session class: zero hits.** The cookie-lane work reads live browser
sessions but no captured cookie value was ever committed.

**Deleted-path class: one file**, `crates/quota-core/src/cache.rs` — the
obsolete TTL cache removed when the background refresher landed. Ordinary
source. No `.cortexkit`, private, strategy or competitor material has ever been
committed here, which is the class the keep-list mechanism exists to kill.

**Issue/PR text: zero hits** across 136 titles, bodies and comments.

## Why no rewrite

The rewrite machinery exists for classes this repo does not have. Subconscious
needed it for 193 deleted `.cortexkit` blobs, a PII blob in fixture history, and
six strategy documents. Here: one deleted source file, zero credentials ever,
zero private docs ever. A filter-repo pass would rotate every commit SHA, break
every citation, and force a fleet re-clone to remove nothing.

Stated as a falsifier rather than a conclusion: **if any credential-class hit, or
any deleted path outside ordinary source, is found in a re-run of this verifier,
the verdict flips to rewrite.** The verifier is the artifact, not this document.

## Tip work required before the flip

1. **LICENSE** — MIT, holder exactly `Ufuk Altinok` (fleet rule ask_cd489a4e).
   Currently absent.
2. **Crate manifests** — no `license` or `repository` field on either crate; both
   `description` fields still say `ai-provider-quota`, the pre-rename name.
3. **README** — overhaul for an external reader: what the module does, the wire
   surface, how to build. No fleet-process internals.
4. **Seat vocabulary** — shallow, ~60 lines across 13 docs: `Alfonso` 30,
   `ALF` 18, `CKCRED` 8, `SUBC` 4. Most `SUBC` hits are the legitimate
   `SUBC_MODULE_ID` environment variable and stay. `docs/charter.md` is the most
   internally-framed doc ("your driver (Alfonso @ subc) reverse-engineered") and
   needs the closest edit.
5. **Actions runs** — 644 to delete before the visibility change; their logs
   carry paths and env.
6. **CI runners** — `matrix.runner`; blacksmith entries move to GitHub-hosted,
   free for public repos.

## Surfaces that need no work

Zero forks (the fork-retains-history problem does not apply), zero releases (no
frozen source tarballs to recreate), zero tags, one PR ever and its text is
clean.
