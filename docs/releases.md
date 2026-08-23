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
   PR when needed. It captures and validates the generated commit's full SHA
   immediately after the commit and before pushing it.
   release-plz compares only the consumer-facing Cargo package with crates.io,
   so repository-only controller changes are deliberately inert. If a
   controller defect requires a fresh patch after the previous crate was
   accepted, advance `package.metadata.mcp-repl.release-recovery-epoch` exactly
   once in that reviewed fix PR. The package-visible epoch has no runtime
   meaning; it makes the fixed source distinct so this same bot-owned
   three-file flow generates the next patch and attributes it in the changelog.
   If the need is discovered only after the fix merges, use an immediate
   reviewed follow-up with a non-skipped conventional commit title that names
   the recovery. Never edit the package version by hand or advance the epoch
   for an ordinary controller change.
2. The trusted `main` workflow discovers at most one open release PR. It
   requires the GitHub Actions bot, the same repository, base `main`, a full
   commit SHA, and exactly `CHANGELOG.md`, `Cargo.toml`, and `Cargo.lock`. The
   discovered live head must equal the generated SHA captured before the push.
   A human-authored version or release PR cannot authorize publication: the bot
   author, parent artifact and run, child status, and exact head and tree checks
   reject lookalikes.
   It preserves a 90-day identity artifact that binds the repository, parent
   run ID and attempt, base SHA, PR number, release branch, and generated head
   SHA for later publication preflight.
3. That workflow posts the required classic `Release gate` status as pending,
   with the exact trusted Release-plz run-attempt URL as its owning claim. A
   normal runner job sends a typed `repository_dispatch` request. GitHub always
   resolves that event's `ci.yml`, local reusable workflows, `GITHUB_REF`, and
   `GITHUB_SHA` from the default branch; no API caller can select workflow code
   from the release branch. The payload binds the parent run and attempt, base
   SHA, PR number, release branch, generated head, and unique claim. Before any
   candidate code runs, the read-only child validates that payload against the
   exact parent attempt, current PR and branch, three-file diff, and durable
   identity artifact. It then fetches the authenticated candidate by its full
   SHA with raw Git commands, verifies detached `HEAD`, and runs the ordinary
   multi-platform CI graph on GitHub-hosted ephemeral runners. Candidate jobs
   receive no secrets or write, OIDC, or cache authority; every shared Rust
   cache read and write is disabled. The parent accepts exactly one bot-owned,
   first-attempt `repository_dispatch` child whose recorded source is the same
   trusted `main` base and whose claim names the candidate head. This avoids
   recursive-event suppression without a PAT or long-lived App credential.
4. The final reporting job posts success only if that exact child run completes
   successfully with one successful aggregate `Release gate`, reports the same
   authenticated candidate SHA, and the pull request is still open at that
   exact head. The
   parent Release-plz attempt remains the status owner's claim while the
   successful status targets the verified child CI attempt URL. The reporter
   requires that its attempt is the same attempt that produced and uploaded the
   identity artifact, revalidates that it is still the current parent attempt
   before each write, and refuses a same-attempt downgrade or replacement. A
   moved or
   untrusted head, stale parent attempt, or mismatched child is refused rather
   than receiving a stale status.
5. The gate includes the ordinary quality, package, dependency, Windows-path,
   and workflow-lint jobs. Exact Rust 1.90.0 checks run natively on the
   manifest's representative Linux, macOS, and Windows targets. Every
   same-repository PR and exact release validation also builds, executes,
   ABI-checks, and packages all manifest targets with stable Rust; untrusted
   fork PRs cannot run that broader native matrix. The native release builder
   deliberately does not use a shared Actions cache, so its final artifacts do
   not inherit mutable cache state from another run.
6. Merge the release PR only after `Release gate` succeeds. The first-party
   merge preflight below makes every ordinary `main` commit ineligible for
   publication, and release-plz itself has publishing disabled.
7. The uncoalesced `release-publish.yml` workflow first proves that its exact
   `main` SHA merged one trusted release-plz PR through a one-parent squash or
   equivalent single-commit rebase. It independently rechecks the
   exact three-file diff and binds the successful classic status to the exact
   bot-owned, default-branch-controlled child CI attempt for that PR head,
   including its one
   successful `Release gate` job. It also verifies the exact successful parent
   Release-plz attempt and reporting job, confirms no newer attempt superseded
   it, and downloads the uniquely named identity artifact whose base, PR,
   branch, and head fields must all match the merge. Only then does a
   credential-free runner packages and compiles the source. A fresh
   least-privilege runner then receives only the crates.io token for a
   fixed-toolchain `cargo publish --locked --no-verify`; no package code
   executes on that runner. A separate read-only first-party job receives no
   registry credential, rebuilds the exact locked `.crate` without executing
   package code, and compares its checksum with crates.io. It deliberately
   creates no GitHub tag or release: those mutations remain deferred until the
   complete release set is ready. This split recovers the partial state where
   crates.io accepted a version before later stages completed, without ever
   colocating registry and GitHub publication authority. After reconciliation,
   `release-publish.yml` calls
   `release-binaries.yml` as a local reusable workflow. The call is resolved
   from the same frozen, trusted `main` `github.sha`; no workflow code is loaded
   from a tag or candidate branch. An exceptional typed recovery dispatch may
   run newer reviewed default-branch code after a failed publication attempt.
   Before exposing the registry credential, it authenticates the owner-sent
   payload, exact failed run and five-job topology, original trusted release
   merge, ancestry, and a strict changed-path allowlist that excludes every
   package, native, container, toolchain, and release-record input. The older
   release merge authorizes recovery but never selects source: Cargo VCS
   metadata and every later public identity bind the recovery event's
   `github.sha`.
8. The binary workflow accepts only one optional recovery-authorization SHA.
   That value is reauthenticated and is never used as a checkout, build, tag,
   record, or attestation source. The workflow otherwise inherits the caller's
   event-bound `github.sha` and `refs/heads/main`, checks out that SHA
   everywhere, derives the semver tag from the locked Cargo metadata, and
   proves that the source is one trusted release merge and that Cargo names one
   canonical version. No tag or draft is required before the build. Native jobs
   use the manifest's exact Rust,
   Python, cargo-auditable, and checksum-pinned musl inputs. They build all
   seven targets, enforce Linux ABI policy, execute each binary, and
   create archives whose paths, modes, timestamps, order, ZIP storage, and raw
   stored gzip stream are independent of host archiver or zlib behavior.
9. A separate read-only attestation matrix verifies each archive checksum,
   creates a deterministic normalized SPDX 2.3 inventory, and signs both SLSA
   v1 provenance and the SPDX predicate. The bundles are verified against the
   exact repository workflow, source digest, trusted `refs/heads/main` source
   ref, and public trusted root before they become workflow artifacts. The
   final annotated tag and canonical release record bind that trusted source
   digest to the release version.
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
    target from the artifacts visible to the current workflow attempt. The
    pinned downloader preserves named directories when a pattern matches
    multiple artifacts, but flattens a sole match into the requested path. The
    selector accepts that flat shape only for one requested logical artifact and
    otherwise requires the attempt-named directory boundary. It accepts exactly
    39 durable assets: archive, checksum, SPDX, provenance bundle, and SBOM
    bundle for each of seven native targets; three equivalent container evidence
    files; and one canonical release record. That record binds the exact source,
    release-target manifest, native file identities, final container digest and
    runnable platform digests. It decodes every DSSE statement and rejects a
    subject or predicate that belongs to anything else.
13. Only after staging smoke and all 39 durable files are complete does a
    package-write job create or verify the immutable version image. It binds
    the record to the same trusted `main` event, source SHA, version, and source
    epoch without depending on a prematurely created GitHub object. One final
    contents-write job then creates or verifies the canonical annotated tag,
    creates or resumes the bot-owned draft, stages the exact 39-file set without
    replacement, verifies it byte for byte, and immediately publishes it with
    GitHub's legacy latest-selection behavior. An extra, duplicate, or
    mismatched asset fails closed; repository release immutability locks the tag
    and assets. Lost responses and failed-job reruns re-read live state and
    succeed only for the same immutable, complete, byte-identical release.
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
release PR plus the latest parent status claim before every write. Pending and
successful statuses use exact `/actions/runs/<id>/attempts/<attempt>` URLs, not
bare run URLs. A pending status points to the claiming Release-plz attempt;
only its verified child CI attempt may become the target of success. The
identity artifact gives publication a durable, independently checked binding
to the same parent attempt and generated candidate. A stale main run, moved
head, newer parent attempt, older claim, or untrusted PR is refused. Candidate
application and build code plus release-plz execute only in credential-free or
`contents: read` jobs; the write-scoped release-PR job is limited to
first-party boundary checks, fixed-artifact verification, and the branch/PR
update.
Ordinary and fork pull-request CI keep their read-only token. Although any
`contents: write` token can request a repository dispatch, an unrelated request
can at most create a duplicate or failing read-only validation run: it cannot
select executable workflow code or acquire an authoritative status claim. The
pipeline needs no personal access token or long-lived GitHub App credential.

Every external Action is pinned to a reviewed full commit SHA and checked by
`scripts/check-actions-pinned.sh`; the trailing version comments let Dependabot
continue proposing implementation updates without changing the manifest's
explicit Rust 1.90.0 MSRV.

Repository settings must keep **Enable release immutability** on. GitHub applies
that policy only to releases created after it is enabled, and the published-
release verifier requires the API's `immutable: true` state before a retry is
accepted. Active `v*` tag rulesets must continue to forbid every update or
deletion. Creation-only enforcement remains a pending hardening item until the
repository has a dedicated, narrowly scoped release principal or GitHub App.
Do not grant the generic repository-wide GitHub Actions App a tag-creation
bypass: that identity is shared by unrelated workflows and is too broad to be
a release boundary. Once a dedicated principal exists, only it should receive
the creation bypass.

GitHub Release and GHCR publication also require dedicated protected publisher
identities or an isolated distribution repository before their write scopes
can be treated as an exclusive authority boundary. The generic source-repository
GitHub Actions identity must receive no publication bypass. Until that
provisioning is complete, keep the local workflow source-bound, minimize the
tag/draft lifetime as above, and retain every live-state verification. This is
tracked in issue #210; dedicated tag creation is tracked in #209.

`release-targets.json` is the authoritative native/container platform, ABI, and
release-tool input contract. `scripts/release-targets.sh` validates its exact
schema and its mirrors in Cargo, Docker, and the installer; workflow runner
labels are accepted only from a fixed allowlist. The same reusable native and
container workflows serve PR rehearsal and publication. Publication binds every
checkout to the calling event's `github.sha`; release rehearsal may accept only
the candidate SHA already authenticated by the default-branch CI control job,
fetches it without a dynamic `actions/checkout` ref, and rechecks detached
`HEAD` before any build.

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
  --source-ref refs/heads/main
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
  --source-ref refs/heads/main
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
- To retry transient release-PR CI, use **Re-run all jobs** on its existing
  Release-plz run. Each attempt must recreate the identity artifact, pending
  claim, default-branch-controlled child CI run, and final report as one
  attempt-scoped chain. **Re-run failed jobs** is intentionally unsupported
  here and fails closed because its retained successful jobs belong to an older
  attempt. If `main` or the release head has advanced, let a new `main` push start a fresh
  Release-plz run instead. The workflow refuses to mutate the generated PR from
  a stale main SHA. If a green release PR is ever held near the 90-day artifact
  limit, use **Re-run all jobs** before merging so publication receives fresh
  identity evidence.
- If **Release publish** fails during merge preflight, credential-free package
  verification, registry publication, or source reconciliation, first use
  **Re-run all jobs** when the frozen workflow code is sound. This retries the
  same source and remains safe if crates.io already accepted it: reconciliation
  requires the rebuilt Cargo archive, embedded VCS SHA, and registry checksum
  to agree exactly. A yanked or mismatched crate fails closed before any tag or
  release exists.
- If the frozen workflow itself is defective and the version is still absent,
  merge a separately reviewed control-plane-only recovery fix to `main`, then
  dispatch the exact failed evidence. For example:

  ```bash
  gh api --method POST repos/joshrotenberg/mcp-repl/dispatches \
    -f event_type=release_publish_recovery \
    -F 'client_payload[schema_version]=1' \
    -f 'client_payload[release_merge_sha]=<40-character release merge SHA>' \
    -F 'client_payload[run_id]=<failed run ID>' \
    -F 'client_payload[run_attempt]=<failed run attempt>'
  ```

  The request is owner-only and cannot select executable code or source. The
  recovery event's current `main` SHA—not the older release merge—becomes the
  Cargo VCS, annotated-tag, release-record, GitHub-attestation, and GHCR source
  identity. Any changed product/build input forces a fresh version. After
  crates.io accepts that recovery SHA, resume later failures by rerunning failed
  jobs in the same recovery run. A later controller commit has different Cargo
  VCS metadata and cannot resume the accepted archive; its checksum mismatch is
  intentionally fatal. Once the final publication boundary begins, a
  noncanonical or moved tag, conflicting draft, or mismatched canonical notes
  also fails closed.
- If crates.io already accepted the version before a later controller or binary
  stage exposed a defect, do not run newer controller code over that accepted
  source and do not synthesize the missing artifacts. Merge the reviewed fix,
  advance `package.metadata.mcp-repl.release-recovery-epoch` exactly once in
  that fix PR, and let the normal Release-plz workflow generate a bot-owned
  fresh patch. If the need is discovered after merge, use the immediate
  reviewed follow-up described above. The epoch is packaged and CI verifies
  that Cargo's normalized manifest preserves it; leaving it unchanged keeps
  ordinary controller-only commits from producing releases.
- A failed native target, container build, attestation, assembly, or staging
  smoke after the crate is published leaves no GitHub tag or release. No
  incomplete binary/container release is public. Content-addressed platform
  objects may remain and every retry verifies them before reuse. Once the exact
  version image has passed, a later tag or GitHub publication failure may leave
  that complete but orphaned public version image; recovery accepts it only at
  the recorded digest. Only an interruption inside the final contents-write job
  can leave a private partial draft, which the retry resumes only with
  byte-identical assets.
- After the release-validation claim has cleared and the local reusable binary
  workflow is running, **Re-run failed jobs** remains supported for transient
  binary-stage failures. A partial rerun retains successful producer artifacts
  from earlier attempts; the workflow selects the newest complete artifact per
  logical target, so successful prior jobs need not rebuild. GitHub deletes the
  earlier artifacts for a full rerun, which recreates the complete current
  attempt instead. In either case, `actions/download-artifact` v5 and later
  extract a sole pattern match directly into the requested path while placing
  multiple matches in artifact-named directories. The selector supports both
  shapes without treating one flattened artifact as multiple logical inputs.
  This is distinct from the attempt-atomic Release-plz validation chain above.
  **Re-run all jobs** remains fail-closed, but regenerated Sigstore bundles are
  not guaranteed to be byte-identical; it may therefore be unable to resume an
  existing partial draft. Prefer **Re-run failed jobs** after final staging has
  begun. The exact assembled release artifact is retained for 90 days to cover
  GitHub's supported rerun window. Duplicate candidates within one attempt,
  missing targets, stale newer attempts, or differing published bytes fail
  closed.
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
./scripts/test-release-recovery.sh
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
