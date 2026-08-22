# Release pipeline

Releases are deliberately two-phase. A failed check or native target must
leave the existing public release untouched, never publish a partial set of
binaries.

## Normal flow

1. A push to `main` runs the checksum-pinned `release-plz update` command on a
   credential-free runner. It must change exactly `CHANGELOG.md`, `Cargo.toml`,
   and `Cargo.lock`; those files and their checksums become a one-day workflow
   artifact. A fresh write-scoped job first proves `main` is still at that
   exact SHA, then verifies and commits only that artifact to a same-repository,
   bot-owned branch whose name starts with `release-plz-`, creating the release
   PR when needed.
2. The trusted `main` workflow discovers at most one open release PR. It
   requires the GitHub Actions bot, the same repository, base `main`, a full
   commit SHA, and exactly `CHANGELOG.md`, `Cargo.toml`, and `Cargo.lock`.
3. That workflow posts the required classic `Release gate` status as pending,
   then calls `ci.yml` as a reusable workflow with the exact candidate SHA and
   a read-only token. This avoids GitHub's suppression of recursively created
   workflow events without introducing a PAT or long-lived App credential.
4. The final reporting job posts success only if the aggregate gate succeeds,
   reports that same SHA, and the pull request is still open at that exact
   head. A moved or untrusted head is refused rather than receiving a stale
   status.
5. The gate includes the ordinary quality, package, dependency, Windows-path,
   workflow-lint, and exact Rust 1.90 MSRV jobs. On a `release-plz-` branch it
   additionally builds and packages all five native release targets.
6. Merge the release PR only after `Release gate` succeeds. The first-party
   merge preflight below makes every ordinary `main` commit ineligible for
   publication, and release-plz itself has publishing disabled.
7. The uncoalesced `release-publish.yml` workflow first proves that its exact
   `main` SHA merged one trusted release-plz PR through a one-parent squash or
   equivalent single-commit rebase. It independently rechecks the
   exact three-file diff and binds the successful classic status to the
   trusted Release-plz run on that PR's base and head. Only then does a
   credential-free runner package and compile the source. A fresh
   least-privilege runner then receives only the crates.io token for a fixed-
   toolchain `cargo publish --locked --no-verify`; no package code executes on
   that runner. A separate first-party job receives `contents: write` but no
   registry credential; it rebuilds the exact locked `.crate` without executing
   package code, compares its checksum with crates.io, then creates or verifies
   the canonical annotated immutable tag and **draft** GitHub release. This
   split closes the partial state where crates.io accepted a version before
   GitHub objects existed, without ever colocating both high-value credentials
   or their runners. A separate job dispatches `release-binaries.yml`.
8. Dispatch carries both the tag and the release-merge SHA. The binary workflow
   must resolve to that SHA, checks it out by hash everywhere, rechecks the live
   tag before publication, and verifies the Cargo version. An active repository
   ruleset allows new `v*` tags but forbids updating or deleting them. The
   workflow rebuilds the five-target matrix through the same reusable workflow
   and `scripts/package-release.sh`; each job stores its archive and checksum as
   a private workflow artifact.
9. Only after all five jobs succeed does the final binary-asset job download
   and verify the complete ten-file set, upload it to the same bot-owned draft,
   re-read GitHub's complete paginated asset set, bind every stored digest and
   size to the local file, and publish the GitHub release. Repository release
   immutability then locks the release, tag, and assets and requires GitHub's
   generated release attestation. Container jobs build both architectures by
   digest. Every version's manifest job is allowed to run: a shared GitHub
   concurrency key would coalesce rather than durably queue pending releases.
   The job creates a version tag only after GHCR returns its exact not-found
   response, and refuses an existing tag unless it contains the run's exact two
   digests. The workflow-level per-tag concurrency key serializes repository
   retries for the same version. It then reconciles `latest` to GitHub's current
   bot-owned immutable release, re-reading that release around the mutable-tag
   write so overlapping versions and older retries converge instead of leaving
   `latest` rolled back. GHCR does not document an atomic create-if-absent or
   immutable-tag control, so package write access must remain restricted to
   this repository; #196 tracks attested digest identity and a registry-native
   immutability boundary. A container failure can leave the source and binary
   release public and the container publication partially complete, but every
   completed workflow write is verified and a retry either resumes safely or
   fails closed.

The status reporter is intentionally narrow and the whole update/validation
graph is serialized. It runs from the trusted default branch, grants only its
reporting jobs `pull-requests: read` and `statuses: write`, and re-reads the
release PR plus the latest status claim before every write. A stale main run,
moved head, older run ID, or untrusted PR is refused. Candidate application and
build code plus release-plz execute only in credential-free or `contents: read`
jobs; the write-scoped release-PR job is limited to first-party boundary
checks, fixed-artifact verification, and the branch/PR update.
Ordinary and fork pull-request CI keep their read-only token, and the pipeline
needs no personal access token or long-lived GitHub App credential.

Every external Action is pinned to a reviewed full commit SHA and checked by
`scripts/check-actions-pinned.sh`; the trailing version comments let Dependabot
continue proposing implementation updates without changing the explicit Rust
1.90 MSRV input.

Repository settings must keep **Enable release immutability** on. GitHub applies
that policy only to releases created after it is enabled, and the published-
release verifier requires the API's `immutable: true` state before a retry is
accepted. The separate `v*` tag ruleset remains defense in depth before a draft
is published.

The native build matrix lives in `.github/workflows/release-build.yml`; both the
PR rehearsal and publication call it. Packaging logic likewise lives in
`scripts/package-release.sh`, and verification/publication in
`scripts/publish-release.sh`. The publication exact-set mirror is
behavior-tested; the authoritative platform manifest tracked in #198 will
replace it and the still-unverified installer mirror.

Those guards decide whether a release becomes public and otherwise run only
while one is being cut, so `scripts/test-release-guards.sh` drives them
against a stubbed `gh`: an unreachable API, an already published release, a
missing archive or checksum, an unexpected local or remote file, a checksum
that does not verify, a mismatched remote digest, and the complete set that
should publish. CI runs it in the workflow-lint job.

## Failure and retry behavior

- A failed release-PR check blocks the merge and therefore blocks crates.io,
  the tag, and the GitHub release.
- To retry transient release-PR CI, use **Re-run failed jobs** on its existing
  Release-plz run; the same run may replace its owned failure after the gate
  passes. If `main` or the release head has advanced, dispatch Release-plz on
  current `main` instead. The workflow refuses to mutate the generated PR from
  a stale main SHA.
- If **Release publish** fails during merge preflight, credential-free package
  verification, registry publication, source reconciliation, or dispatch, use
  **Re-run all jobs** on that existing run. This retries a registry attempt
  that failed before upload while remaining safe if crates.io already accepted
  it. The event stays bound to the release-merge SHA; a fresh dispatch from a
  later `main` commit is intentionally ineligible. Reconciliation resumes a
  crate-only partial publication only after proving the exact archive's VCS and
  registry checksums; a yanked or mismatched crate, noncanonical or moved tag,
  conflicting draft, or mismatched canonical notes fails closed.
- A failed native binary target after the crate is published leaves the GitHub
  release in draft form. No incomplete binary release is public. A container
  failure occurs after source and binary publication. It may leave the
  version manifest present while `latest` is still unchanged; the
  manifest job refuses a pre-existing version tag that differs from its exact
  architecture digests.
- Rerun **Release binaries** at the existing immutable tag after a transient
  native or container failure with **Re-run all jobs**. GitHub artifacts are
  scoped to a run attempt, so rerunning only failed jobs cannot reliably reuse
  successful native packages or architecture digests from the prior attempt,
  regardless of their retention period. The manifest job refuses anything
  other than two distinct, regular SHA-256 digest filenames and will fail
  closed if a fresh rebuild differs from an already-published version manifest.
  The publication job downloads already-public immutable binary assets and
  verifies their exact names, self-bound checksums, API digests, sizes, and
  upload state; it does not assume a fresh native rebuild is byte-identical. An
  older tag retry reconciles `latest` to GitHub's current immutable release, not
  to itself. The workflow runs from the immutable tag, so a later `main`
  workflow cannot silently attest different source while publishing old
  binaries. If an existing container version conflicts or the tagged pipeline
  itself is defective, stop and use a separately reviewed recovery workflow;
  never replace a version tag or move the source tag.
- Never manually publish the draft to work around a failed target. That would
  bypass the complete-set guarantee.

## Local checks

The regular Rust checks are in `CONTRIBUTING.md`. Workflow and installer
changes additionally require:

```bash
actionlint
./scripts/check-actions-pinned.sh
shellcheck install.sh scripts/*.sh
sh -n install.sh
for script in scripts/*.sh; do bash -n "$script"; done
./scripts/test-release-guards.sh
./scripts/test-release-workflow.sh
./scripts/test-source-release.sh
./scripts/test-container-manifest.sh
./scripts/test-source-package.sh
```

CI pins actionlint 1.7.12, verifies its reviewed hardcoded checksum, and uses the
ShellCheck version installed on GitHub's Ubuntu runner.
