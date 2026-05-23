# Releasing

Releases are produced by [`.github/workflows/release.yml`](../.github/workflows/release.yml),
triggered when a `v*` tag is pushed. The workflow builds every binary for all
supported targets, bundles them into per-platform tarballs with SHA-256
checksums, and publishes a GitHub Release with auto-generated release notes.

## Versioning

The single source of truth is `version` under `[workspace.package]` in the
root `Cargo.toml`. Every binary inherits it, so `agentd --version` and
`agent --version` always report the workspace version. Use [semver](https://semver.org/)
(`MAJOR.MINOR.PATCH`).

The release workflow's `verify` job refuses to build unless the pushed tag
(minus its leading `v`) exactly matches the Cargo version — a mistyped tag can
never publish a mislabelled binary.

## Cutting a release

1. Bump the version in `Cargo.toml`:

   ```toml
   [workspace.package]
   version = "0.2.0"
   ```

   Commit it (and run `cargo build` once so `Cargo.lock` updates), open a PR,
   and merge it to `main` as usual.

2. Tag the merge commit and push the tag:

   ```sh
   git checkout main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The workflow runs. When it finishes, a GitHub Release for `v0.2.0` exists
   with these assets:

   - `agentd-aarch64-apple-darwin.tar.gz`     (macOS, Apple Silicon)
   - `agentd-x86_64-apple-darwin.tar.gz`      (macOS, Intel)
   - `agentd-x86_64-unknown-linux-musl.tar.gz` (Linux x86_64, static)
   - `agentd-aarch64-unknown-linux-gnu.tar.gz` (Linux arm64)
   - `SHA256SUMS`

   Each tarball contains all release binaries (`agent`, `agentd`,
   `agentd-mcp`, `agentd-adapter-*`) plus `README.md` and `LICENSE`.

4. Review the release notes. The workflow passes `generate_release_notes: true`
   to the release step, so GitHub fills the release body automatically from the
   pull requests merged since the previous `v*` tag — a categorized changelog
   with contributor credits and a "Full Changelog" compare link. Two
   consequences worth knowing:

   - **PR titles _are_ the changelog.** Each line comes from a merged PR's
     title, so clear, conventional titles (`feat(cli): …`, `fix(daemon): …`)
     produce clean notes. This is also why we squash-merge — one tidy line per
     change.
   - **You can edit after publishing.** Open the release on the
     [releases page](https://github.com/zarvis-ai/agentd/releases), click
     _Edit_, and add a summary, highlights, breaking-change callouts, or upgrade
     notes on top of the generated list.

   To group the auto-generated notes into sections (Features / Fixes / …) or
   exclude certain labels, add a [`.github/release.yml`](https://docs.github.com/en/repositories/releasing-projects-on-github/automatically-generated-release-notes#configuring-automatically-generated-release-notes)
   with category rules; the release step picks it up with no workflow change.

## What ships, and why together

The daemon locates an adapter by looking next to its own binary first (see
`locate_binary` in `crates/daemon/src/adapter.rs`), then falling back to
`PATH`. So the release bundles all eight binaries, and `install.sh` installs
them into one directory. Splitting them up makes the daemon fail with
"adapter binary not found".

## Testing the build without releasing

Run the workflow manually from the Actions tab (`workflow_dispatch`). It runs
the full build matrix and uploads the tarballs as workflow artifacts, but the
`release` job is skipped (it only runs for `v*` tags), so nothing is published.
