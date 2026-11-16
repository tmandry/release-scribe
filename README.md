# release-scribe

`release-scribe` generates LLM-assisted release notes from your Git history and GitHub pull requests. It collects your commits and diffs, drafts notes using your project's style guide, checks that every commit is accounted for, and caches outputs across runs. You can also use it to open an isolated draft PR so you can review the notes before publishing.

You'll need Git and the GitHub CLI (`gh`) installed. Keep your project's guidelines and configuration in the repository you're generating notes for.

## Local use

Build the CLI and run the pipeline step by step:

```sh
cargo build --locked
target/debug/release-scribe collect --repo /path/to/project --github-repo owner/project \
  --base v1.0.0 --target SOURCE_SHA --guidelines guidelines.md \
  --model gpt-6.1-sol --omit CHANGELOG.md --output run/input

# Optional: pull in PR descriptions, comments, and reviews, then bundle a final
# request using --github-context run/github.json.
target/debug/release-scribe github --request run/input/request.json --output run/github.json

# Reads OPENAI_API_KEY from your environment. Makes one call on a cache miss.
target/debug/release-scribe generate --request run/input/request.json \
  --cache run/cache --output run/notes

```

There are also a few helper subcommands: `accept` validates JSON generated outside the tool, `restore` renders a cached run without calling a model, and `fingerprint` prints a request's cache key. Check `--help` for all flags and `prompt.md` for the expected response format.

**How it works under the hood:**

* **Collection:** Gathers commits, patches, and the net diff between your base and target refs. You can omit noisy files from the payload, though commit messages and PR descriptions are still sent as-is.
* **Validation:** Checks that every commit is covered once, all references match up, and any documentation links exist in your supplied catalog. This keeps the output grounded in your actual changes, though you'll still want to read through the final notes for accuracy and tone.

## Isolated PR evaluation

The `evaluate` command lets you test release notes on isolated branches without touching your normal release workflow:

```sh
# Dry run: collect the request payload without calling a model or touching GitHub.
target/debug/release-scribe evaluate --repo /path/to/project --config config.json --output run

# Full run: call the model on a cache miss and update the isolated draft PR.
target/debug/release-scribe evaluate --repo /path/to/project --config config.json --output applied --apply

```

See [integration](https://www.google.com/search?q=docs/integration.md) for details on configuration, permissions, caching, and rollout setup.

## GitHub Actions example

To run previews in CI, commit your style guidelines to `docs/release-notes.md` and add `.github/release-scribe.json` to your repository:

```json
{
  "repository": "owner/project",
  "source_branch": "main",
  "guidelines": "docs/release-notes.md",
  "model": "gpt-6.1-sol",
  "omit": ["CHANGELOG.md", ".release-scribe"]
}

```

Replace `owner/project` with your repository. See [integration](https://www.google.com/search?q=docs/integration.md) if you want to include extra context files or verified documentation links.

Next, save this workflow as `.github/workflows/release-notes.yml` on your default branch:

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

Before running the workflow, configure two repository settings:

1. Go to **Settings → Environments**, create a `release-notes` environment restricted to the `main` branch, and add `OPENAI_API_KEY` as an environment secret. Remove any repo-wide copy of the key so it isn't exposed to other workflows.
2. In your repository's Actions settings, allow GitHub Actions to create pull requests.

Your repository also needs at least one published release tag to diff against. When you trigger the workflow from the Actions tab, leaving **apply** unchecked runs a dry run without calling a model or writing to GitHub. Checking **apply** generates the notes and updates the isolated draft PR for review (leave it unmerged while testing). Your existing release pipeline continues to run independently.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo package --list --allow-dirty

```

Tests use local Git fixtures and mock transports, so running the suite makes no live API calls.

## License

Licensed under either [Apache 2.0](https://www.google.com/search?q=LICENSE-APACHE) or [MIT](https://www.google.com/search?q=LICENSE-MIT), at your option.
