# release-scribe

A Rust tool for model-assisted release notes: deterministic Git evidence,
structured OpenAI generation, validated source coverage, immutable caching,
and an isolated draft PR for evaluation.

Requires Git and Rust to build; the compiled tool uses `curl` for HTTPS and `gh`
for GitHub. There is no Python or Node runtime. Project guidelines and configuration
belong in the consuming repository. This repository contains no project evaluations.

## Local use

```sh
cargo build --locked
target/debug/release-scribe collect --repo /path/to/project --github-repo owner/project \
  --base v1.0.0 --target SOURCE_SHA --guidelines guidelines.md \
  --model gpt-6.1-sol --omit CHANGELOG.md --output run/input

# Optional: fetch paginated PR descriptions, comments, and reviews, then collect
# a final request using --github-context run/github.json.
target/debug/release-scribe github --request run/input/request.json --output run/github.json

# Read OPENAI_API_KEY from the environment. One call on a cache miss; no retries.
target/debug/release-scribe generate --request run/input/request.json \
  --cache run/cache --output run/notes
```

`accept` validates externally generated JSON; `restore` renders an exact cache
hit with no provider call. `fingerprint` prints a request's content key. See
`--help` for all options and `prompt.md` for the response contract.

Collection pins Git refs, checks ancestry, inventories merge and direct commits,
and supplies per-commit patches and a net diff. Context files are read from both
endpoints. Omissions remove file contents while keeping coverage identities;
they do not redact secrets from commit messages or PR bodies. Input limits fail
instead of truncating. Standard GitHub merge messages provide PR association;
squash/rebase association is not implemented. Missing context is explicit.

Validation requires every source commit exactly once, known IDs, consistent
bullet/coverage references, and questions for unresolved coverage. Documentation
links must exactly match the supplied catalog. These checks establish coverage
and references; factual correctness and editorial quality still need review.

## Isolated PR evaluation

```sh
# Free preparation: collect a request without generation or GitHub mutations.
target/debug/release-scribe evaluate --repo /path/to/project --config config.json --output run

# Allow one model call on a miss and writes to the reserved evaluation branches.
target/debug/release-scribe evaluate --repo /path/to/project --config config.json --output applied --apply
```

The consumer selects the model. Sol requests use the Responses API with strict
JSON output, high reasoning effort, a 24,000 output token limit, and storage
disabled. Keys are passed through transport stdin and never saved in requests,
caches, diagnostics, or command arguments. Requests send selected source and
discussion to OpenAI; model output is never executed.

See [integration](docs/integration.md) for configuration, permissions, durable
caching, human edits, and rollout boundaries.

## GitHub Actions example

In the consuming repository, commit your editorial guidelines to
`docs/release-notes.md` and add `.github/release-scribe.json`:

```json
{
  "repository": "owner/project",
  "source_branch": "main",
  "guidelines": "docs/release-notes.md",
  "model": "gpt-6.1-sol",
  "omit": ["CHANGELOG.md", ".release-scribe"]
}
```

Replace `owner/project` with your repository. Add context files and verified
documentation links as described in [integration](docs/integration.md) so the
model can check adoption instructions against the source.

Save this as `.github/workflows/release-notes.yml` on your default branch:

```yaml
name: Release notes preview
on:
  workflow_dispatch:
    inputs:
      apply:
        description: Generate notes and update the isolated draft PR
        type: boolean
        default: false

permissions:
  contents: write
  pull-requests: write
  statuses: write

concurrency:
  group: release-scribe-evaluation
  cancel-in-progress: false

jobs:
  preview:
    if: github.ref == 'refs/heads/main'
    environment: release-notes
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
        with:
          ref: main
          fetch-depth: 0
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - name: Install and cache release-scribe
        uses: baptiste0928/cargo-install@v3
        with:
          crate: release-scribe
          git: https://github.com/tmandry/release-scribe
          commit: efd6db1066073e76b884db76573706966d9019c7 # Pin a reviewed commit.
          locked: true
      - name: Prepare or apply the preview
        env:
          GH_TOKEN: ${{ github.token }}
          OPENAI_API_KEY: ${{ inputs.apply && secrets.OPENAI_API_KEY || '' }}
          APPLY: ${{ inputs.apply }}
        run: |
          set -euo pipefail
          args=()
          if [ "$APPLY" = true ]; then args+=(--apply); fi
          release-scribe evaluate \
            --repo "$GITHUB_WORKSPACE" \
            --config .github/release-scribe.json \
            --output "$RUNNER_TEMP/release-scribe" "${args[@]}"
```

Create a `release-notes` environment in the consuming repository's Settings →
Environments. Restrict it to the `main` branch, with no allowed tags, and add
`OPENAI_API_KEY` as an **environment secret**. If a repository-level copy of the
key already exists, remove that copy after adding the environment secret. The
workflow's `if` condition is an extra guard; the environment's branch policy
enforces which refs can access the key. Other trusted workflows on `main` could
also request this environment.

Allow Actions to create pull requests in the repository's workflow settings.
The install action builds from the pinned Git commit and caches the executable
for later runs.

The repository needs a published stable release and its tag. An unchecked
**apply** collects evidence without a model call or GitHub writes. Checking it
allows one model call on a cache miss and updates the isolated draft PR. Review
that PR without merging it while evaluating the flow. The executable cache above
is separate from validated notes stored on the dedicated state branch. The
existing release pipeline continues to run independently.

## Development and publication

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo package --list --allow-dirty
```

Tests use temporary Git repositories, a fixture GitHub service, structured
responses, and a fake HTTPS transport. They make no paid calls. CI runs on Linux
and macOS. The crate has an explicit package allowlist.

Project-specific guidelines, configuration, and evaluation results belong in the
consuming project rather than this repository or its crate package.

## License

Licensed under either [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
