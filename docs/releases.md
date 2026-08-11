# Release pipeline

Releases are deliberately two-phase. A failed check or native target must
leave the existing public release untouched, never publish a partial set of
binaries.

## Normal flow

1. A push to `main` runs release-plz in `release-pr` mode. It creates or
   updates a branch whose name starts with `release-plz-`.
2. Because GitHub does not automatically run ordinary workflows for pushes
   made with `GITHUB_TOKEN`, the release-plz workflow explicitly dispatches
   `ci.yml` at that branch. GitHub documents `workflow_dispatch` as an
   exception to the token's workflow-recursion protection.
3. The required `Release gate` check includes the ordinary quality, package,
   dependency, Windows-path, workflow-lint, and exact Rust 1.90 MSRV jobs. On
   a `release-plz-` branch it additionally builds and packages all five native
   release targets.
4. Merge the release PR only after `Release gate` succeeds. With
   `release_always = false`, no other commit is eligible for publication.
5. Release-plz publishes the crate, creates the tag, and creates a **draft**
   GitHub release. It then dispatches `release-binaries.yml` for that tag.
6. The binary workflow rebuilds the same five-target matrix through the same
   reusable workflow and `scripts/package-release.sh`. Each job stores its
   archive and checksum as a private workflow artifact.
7. Only after all five jobs succeed does the final job download and verify the
   complete ten-file set, attach it to the draft, and publish the GitHub
   release.

The release target matrix lives only in
`.github/workflows/release-build.yml`; both the PR rehearsal and publication
call it. Packaging logic likewise lives only in `scripts/package-release.sh`,
and the verification and publication step in `scripts/publish-release.sh`.

Those guards decide whether a release becomes public and otherwise run only
while one is being cut, so `scripts/test-release-guards.sh` drives them
against a stubbed `gh`: an unreachable API, an already published release, a
missing archive or checksum, an unexpected extra file, a checksum that does
not verify, and the complete set that should publish. CI runs it in the
workflow-lint job.

## Failure and retry behavior

- A failed release-PR check blocks the merge and therefore blocks crates.io,
  the tag, and the GitHub release.
- A failed target after the crate is published leaves the GitHub release in
  draft form. No incomplete binary release is public.
- Rerun **Release binaries** with the existing draft tag after a transient
  runner failure. If the pipeline itself needs a fix, merge that fix through a
  normal PR first, then dispatch the workflow from `main` with the draft tag.
  Reruns replace draft assets and publish only after all checks pass.
- Never manually publish the draft to work around a failed target. That would
  bypass the complete-set guarantee.

## Local checks

The regular Rust checks are in `CONTRIBUTING.md`. Workflow and installer
changes additionally require:

```bash
actionlint
shellcheck install.sh scripts/package-release.sh
sh -n install.sh
bash -n scripts/package-release.sh
```

CI pins actionlint 1.7.12, verifies its published checksum, and uses the
ShellCheck version installed on GitHub's Ubuntu runner.
