# Manual evaluation integration

Keep the existing release pipeline running. Evaluation writes only
`release-scribe/evaluation` and `release-scribe-state-evaluation`. The preview PR
contains `.release-scribe/` files and a no-merge guard. It changes no versions,
changelogs, release labels, tags, or releases. New PRs are drafts; updates omit
the draft field. Only the maintainer changes readiness.

Supply this JSON from the consuming repository:

```json
{
  "repository": "owner/project",
  "source_branch": "main",
  "guidelines": "docs/release-notes.md",
  "model": "gpt-6.1-sol",
  "omit": ["CHANGELOG.md", ".release-scribe"],
  "context": ["defaults.toml"],
  "documentation_links": {"Configuration": "https://example.com/config/"}
}
```

The checkout must match the current source branch. Fetch full history and tags.
The last published stable GitHub release determines the base; its tag must agree
locally and remotely. Guidelines come from the pinned source commit, not an
evaluation branch. Run from a trusted `workflow_dispatch` workflow during
evaluation. Do not run with credentials on untrusted PR events.

Create a `release-notes` environment restricted to the `main` branch, with no
allowed tags. Store `OPENAI_API_KEY` there as an environment secret and set
`environment: release-notes` on the evaluation job. Remove any repository-level
copy of this secret. An environment reference alone does not restrict refs;
configure its branch policy in repository settings. Other trusted workflows on
the allowed branch can also request this environment.

`GH_TOKEN` needs same-repository
contents, pull-request, and commit-status write access. Disable persisted checkout
credentials. Pin the tool installation to the reviewed commit SHA. Serialize runs
with one concurrency group and `cancel-in-progress: false`.

## Caching and recovery

Keys cover exact instructions, guidelines, model/provider/settings, source
evidence, documentation catalog, and human edits and feedback. Generated notes
and run timestamps are not self-fed into the next request. Identical inputs reuse
a validated response on a fresh runner. The state branch holds responses and
usage; Actions artifacts/cache are optional diagnostics, not the durable record.
Give the state branch the same access controls as the consuming project's source.

The provider makes one bounded call on a miss, with no automatic retry or fallback.
It validates the result, persists it on the remote state branch, and only then
updates the preview. A failed PR write can resume from the saved response. A crash
or failed state write immediately after a paid response can still require another
call; exactly-once billing is not guaranteed.

Branch updates are fast-forward only, with a head recheck. Source/release changes
leave the result cached and abort PR application. Unrelated files on the preview
branch cause a refusal to overwrite. Do not delete the state branch between runs.

## Editorial updates

Both `notes.md` and the managed PR-body block are compared with the last generated
text. Human edits, including removed bullets, remain untouched; `proposed.md`
holds a replacement for manual review. Text outside the managed block is preserved.
Removed or duplicate markers require manual reconciliation. Human comments and
reviews become untrusted evidence on the next manual run; they do not automatically
trigger generation. No source-PR comments are posted during this stage.

The guard fails the evaluation PR's check; the command also posts a failing
`release-scribe/no-merge` status. Configure the guard as a required check in
repository rules for enforcement beyond draft status. A failing optional check
cannot prevent an administrator from merging. The tool never calls merge or
release-publication APIs.

Before enabling automatic events or replacing release automation, separately
test feedback races, budget enforcement, versioning, snapshots, exact-tag
publication, and source-PR comment updates. This manual stage does not implement
those production steps.


## Input budgets

Evaluation defaults to an 800,000-byte input ceiling. Set `max_input_bytes` in
the consuming repository's configuration to choose a different ceiling, for
example `"max_input_bytes": 2000000` for a larger release range. Collection also
supports `--max-bytes`. The default remains conservative; a byte ceiling is not
a provider token limit, so choose it for the selected model and spending budget.

Dry runs report the input bytes, configured limit, commit count, and raw section
sizes. Oversize failures identify section sizes without logging source contents.
The tool never truncates evidence or invokes a model when the ceiling is exceeded.
Changing the ceiling alone does not change model inputs or invalidate cached notes.
