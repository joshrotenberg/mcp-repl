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
   and workflow-lint jobs. Exact Rust 1.90.0 checks run natively on the
   manifest's representative Linux, macOS, and Windows targets. Every
   same-repository PR and exact release validation also builds, executes,
   ABI-checks, and packages all manifest targets with stable Rust; untrusted
   fork PRs cannot run that broader native matrix.
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
   checks out that SHA everywhere, rechecks the immutable annotated tag, and
   verifies Cargo's version. An active repository ruleset allows new `v*` tags
   but forbids updating or deleting them. Native jobs use the manifest's exact
   Rust, Python, cargo-auditable, and checksum-pinned musl inputs. They build all
   seven targets, enforce Linux ABI policy, execute each binary, and create
   archives whose paths, modes, timestamps, order, ZIP storage, and raw stored
   gzip stream are independent of host archiver or zlib behavior.
9. A separate read-only attestation matrix verifies each archive checksum,
   creates a deterministic normalized SPDX 2.3 inventory, and signs both SLSA
   v1 provenance and the SPDX predicate. The bundles are verified against the
   exact repository workflow, source digest, tag ref, and public trusted root
   before they become workflow artifacts.
10. Container jobs use digest-pinned base, BuildKit, and SBOM-generator images
    plus a checksum-pinned Buildx binary. Each architecture builds natively,
    rewrites layer timestamps to the source commit epoch, pushes by digest, and
    is executed by its runnable digest. The job hashes the raw registry index
    and every mapped attestation manifest, checks descriptor sizes and subjects,
    and requires aggregate BuildKit SLSA and SPDX predicates before claiming
    that evidence in its platform metadata.
11. The index job accepts exactly one metadata file per supported platform,
    stages their attested multi-platform index at `sha-<source SHA>`, and creates
    a deterministic index SPDX document. GitHub then signs SLSA v1 provenance
    and the SPDX 2.3 predicate for the final index digest and also attaches both
    to GHCR. Both the downloaded bundles and registry referrers are verified.
    An anonymous job pulls and executes the content-addressed staging index
    before any public release transition.
12. The assembly job selects the newest complete artifact for each logical
    target across all attempts in the same workflow run. It accepts exactly 39
    durable assets: archive, checksum, SPDX, provenance bundle, and SBOM bundle
    for each of seven native targets; three equivalent container evidence
    files; and one canonical release record. That record binds the exact source,
    release-target manifest, native file identities, final container digest and
    runnable platform digests. It decodes every DSSE statement and rejects a
    subject or predicate that belongs to anything else.
13. Publication stages the exact 39-file set on the bot-owned draft without
    replacement. A retry may fill a byte-identical partial subset, but an extra,
    duplicate, or mismatched asset fails closed. While the release is still a
    trusted draft, the workflow creates or verifies the immutable version image.
    It then publishes the draft with GitHub's legacy latest-selection behavior;
    repository release immutability locks its tag and assets. Lost responses and
    failed-job reruns re-read live state and succeed only for the same immutable,
    complete, byte-identical release.
14. After publication, `latest` is reconciled to GitHub's current bot-owned
    immutable latest release. The workflow re-reads that release around the
    mutable-tag write so overlapping versions and older retries converge instead
    of rolling it back. GHCR does not expose an immutable-tag policy, so package
    write access stays restricted to this repository and every version/latest
    read is compared to raw registry bytes by digest. A final anonymous job
    executes both the version and `latest` images and proves that their raw
    indexes identify the release record's digest.

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
continue proposing implementation updates without changing the manifest's
explicit Rust 1.90.0 MSRV.

Repository settings must keep **Enable release immutability** on. GitHub applies
that policy only to releases created after it is enabled, and the published-
release verifier requires the API's `immutable: true` state before a retry is
accepted. The separate `v*` tag ruleset remains defense in depth before a draft
is published.

`release-targets.json` is the authoritative native/container platform, ABI, and
release-tool input contract. `scripts/release-targets.sh` validates its exact
schema and its mirrors in Cargo, Docker, and the installer; workflow runner
labels are accepted only from a fixed allowlist. The same reusable native and
container workflows serve PR rehearsal and publication.

Packaging lives in `scripts/package-release.sh`, Linux ABI enforcement in
`scripts/verify-release-binary.sh`, container evidence in
`scripts/publish-container-manifest.sh`, canonical identity assembly in
`scripts/build-release-record.sh`, and GitHub publication in
`scripts/publish-release.sh`. Their behavior suites exercise races, malformed
schemas, mixed-attempt selection, partial uploads, lost responses, moved tags,
unrelated SPDX documents, swapped DSSE subjects and predicates, missing OCI
attestations, and byte-different retries. CI runs those suites in the
workflow-lint job.

## Verify a downloaded release

The checksum is the quickest integrity check; the bundles add source/workflow
identity. Download the archive, its four adjacent evidence files, and the
canonical release record, then run:

```bash
archive=mcp-repl-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
tag=vX.Y.Z
record=mcp-repl-$tag-release.json
source_sha=$(jq -r '.source_sha' "$record")
common=(
  --repo joshrotenberg/mcp-repl
  --signer-workflow joshrotenberg/mcp-repl/.github/workflows/release-binaries.yml
  --source-ref "refs/tags/$tag"
  --source-digest "$source_sha"
  --deny-self-hosted-runners
)
sha256sum --check "$archive.sha256"
gh attestation verify "$archive" \
  --bundle "$archive.provenance.sigstore.json" \
  --predicate-type https://slsa.dev/provenance/v1 \
  "${common[@]}"
gh attestation verify "$archive" \
  --bundle "$archive.sbom.sigstore.json" \
  --predicate-type https://spdx.dev/Document/v2.3 \
  "${common[@]}"
```

The release record names the final container index rather than a mutable tag:

```bash
tag=vX.Y.Z
record=mcp-repl-$tag-release.json
source_sha=$(jq -r '.source_sha' "$record")
digest=$(jq -r '.container.manifest_digest' "$record")
subject="oci://ghcr.io/joshrotenberg/mcp-repl@$digest"
common=(
  --repo joshrotenberg/mcp-repl
  --signer-workflow joshrotenberg/mcp-repl/.github/workflows/release-binaries.yml
  --source-ref "refs/tags/$tag"
  --source-digest "$source_sha"
  --deny-self-hosted-runners
)
gh attestation verify "$subject" \
  --bundle "mcp-repl-$tag-container.provenance.sigstore.json" \
  --predicate-type https://slsa.dev/provenance/v1 \
  "${common[@]}"
gh attestation verify "$subject" \
  --bundle "mcp-repl-$tag-container.sbom.sigstore.json" \
  --predicate-type https://spdx.dev/Document/v2.3 \
  "${common[@]}"
```

For a high-assurance audit, also hash every downloaded durable asset and compare
its name, size, and SHA-256 with the corresponding identity in the canonical
compact release record.

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
- A failed native target, container build, attestation, assembly, or staging
  smoke after the crate is published leaves the GitHub release in draft form.
  No incomplete binary/container release is public. Content-addressed platform
  objects or a private partial draft may remain, but every retry verifies them
  before reuse.
- For transient failures in **Release binaries**, **Re-run failed jobs** is
  supported. The workflow downloads all same-run artifact attempts and selects
  the newest complete attempt per logical target, so successful prior jobs need
  not rebuild. **Re-run all jobs** remains safe when a complete rebuild is
  desired. Duplicate candidates within one attempt, missing targets, stale
  newer attempts, or differing published bytes fail closed.
- A retry after draft publication re-reads live state. It accepts only the same
  bot-owned immutable release with the exact 39 byte-identical assets; it never
  trusts a stale prerequisite output or replaces an asset. An older tag retry
  reconciles `latest` to GitHub's current immutable latest release, not itself.
  If an existing container version conflicts or the tagged pipeline itself is
  defective, stop and use a separately reviewed recovery workflow; never move a
  source tag, replace a version image, or disable release immutability.
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
./scripts/test-publish-release.sh
./scripts/test-release-workflow.sh
./scripts/test-source-release.sh
./scripts/test-container-manifest.sh
./scripts/test-container-sbom.sh
./scripts/test-release-record.sh
./scripts/test-select-run-artifacts.sh
./scripts/test-normalize-release-sbom.sh
./scripts/test-release-targets.sh
./scripts/test-installer.sh
./scripts/test-package-release.sh
./scripts/test-source-package.sh
```

CI pins actionlint 1.7.12, verifies its reviewed hardcoded checksum, and uses the
ShellCheck version installed on GitHub's Ubuntu runner.
