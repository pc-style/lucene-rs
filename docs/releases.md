# Releases are manual

Pushing commits or tags never publishes a crate. Only the **Release → Run workflow** action
can publish. The workflow runs on `main`, checks the requested version against `Cargo.toml`,
runs the full Linux/macOS/Windows CI matrix, then verifies the package before publishing.
The GitHub release tag points at the same commit that passed those checks, even if `main`
moves while they run. Releases are serialized and an active publish is never cancelled.

## Setup

Add a crates.io token with permission to publish `lucene-rs` as the repository Actions secret
`CARGO_REGISTRY_TOKEN`. Keep it out of commits, workflow inputs and logs. The workflow's
`GITHUB_TOKEN` creates the GitHub release; no personal GitHub token is needed.

## Publish

1. Update `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, then push to `main`.
2. Open **Actions → Release → Run workflow**, choose `main`, and enter the exact version
   without `v`. Leave **dry_run** checked for a rehearsal: it runs all checks without
   publishing, creating tags or creating a GitHub release.
3. Run again with **dry_run** unchecked to publish to crates.io, then create `vVERSION`
   and a GitHub release with generated notes and the `.crate` archive attached.

Equivalent CLI commands:

```sh
gh workflow run release.yml --ref main -f version=0.1.0 -F dry_run=true
gh workflow run release.yml --ref main -f version=0.1.0 -F dry_run=false
```

Versions on crates.io are immutable. If upload succeeds but GitHub release creation fails,
do not republish or move an existing tag: inspect the run, verify the published crate, and
create the missing GitHub release at that run's commit. A repeated publish of the same
version fails rather than silently attaching a different source commit to an existing crate.
