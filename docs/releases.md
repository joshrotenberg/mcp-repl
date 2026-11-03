# Releases

mcp-repl uses release-plz for versioning and crates.io publication, cargo-dist
for native artifacts and the GitHub Release, and Docker's official actions for
the container image. The project does not maintain a parallel release task
runner or script suite.

## Release flow

1. A push to `main` runs `.github/workflows/release-plz.yml`.
2. release-plz opens or updates a conventional release PR containing the next
   version and changelog. Because a PR created by `GITHUB_TOKEN` does not start
   workflows automatically, the release workflow explicitly dispatches
   `ci.yml` for that branch.
3. Review the release PR and require the normal CI and container checks before
   merging it.
4. On the resulting push to `main`, release-plz publishes the crate through
   crates.io trusted publishing and creates the exact release tag.
5. The workflow dispatches the current cargo-dist workflow on `main`, passing
   that tag. The plan resolves it once to a source SHA used by every build and
   publication job. cargo-dist builds the native
   archives, checksums, shell and Homebrew installers, SBOM, and GitHub
   attestations, then creates the GitHub Release and updates the formula in
   `joshrotenberg/homebrew-brew`.
6. The same tagged source is built for `linux/amd64` and `linux/arm64` and
   published to GHCR as the version and `latest` tags with BuildKit provenance
   and SBOM attestations.

The native target list and artifact policy live in `dist-workspace.toml`.
`cargo dist generate` owns `.github/workflows/release.yml`; this repository
keeps the generated workflow's default permission read-only and grants write
permissions only to its host job.
Preserve the source-SHA wiring and publication deadline checks when regenerating
the workflow; `allow-dirty = ["ci"]` permits these reviewed patches.

Default GitHub attestations identify the workflow's `main` ref and SHA; they
do not independently attest the checked-out release-tag source. The plan
records the tag's resolved source SHA separately and passes it to each build
and publication job. When rebuilding an older tag, do not treat the attested
workflow SHA as the release source SHA.

## Repository setup

GitHub Actions' default workflow permission stays read-only. The repository
must allow GitHub Actions to create and approve pull requests so release-plz
can maintain its PR.

Configure a crates.io trusted publisher for:

- repository: `joshrotenberg/mcp-repl`
- workflow: `release-plz.yml`
- environment: `crates-io`

Protect the `crates-io` environment so only the `main` branch can deploy to it.
No long-lived crates.io token is needed.

Add a repository Actions secret named `HOMEBREW_TAP_TOKEN`. It should be a
fine-grained personal access token limited to `joshrotenberg/homebrew-brew`,
with repository Contents permission set to read and write. cargo-dist uses it
only to commit each release's formula to the tap.

## Local checks

Run the same core checks used by CI:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo package --locked
python3 scripts/check_advisory_expiry.py
cargo deny check -D unmatched-skip -D unnecessary-skip \
  advisories licenses bans sources
cargo dist plan
```

The temporary exceptions for the three unmaintained dependencies in [#309](https://github.com/joshrotenberg/mcp-repl/issues/309)
expire on November 16, 2026 at midnight Pacific (08:00 UTC). Publication jobs
load the guard from trusted `main` and check the exact source being released,
including its lockfile. Older tags without the guard or an ignore list cannot
bypass the cutoff while still containing the affected packages.

Fresh clock checks precede crate publication, native hosting, attestation,
GitHub Release creation, Homebrew push, and each container registry copy.
Containers are built as an OCI archive before the check, with multiarch SBOM
and provenance retained; Skopeo copies all manifests without changing digests.
The guard permits a source with neither affected packages nor exceptions after
the cutoff. It has no time override.

For container changes, build and smoke the image:

```bash
docker build -t mcp-repl:local .
docker run --rm mcp-repl:local --version
```

## Verify a downloaded release

Download an archive and its `.sha256` file from the GitHub Release, then run
the platform checksum tool. For example:

```bash
sha256sum --check mcp-repl-*.tar.gz.sha256
```

On macOS, use `shasum -a 256 -c` instead. GitHub's CLI can verify the artifact
attestation against this repository:

```bash
gh attestation verify mcp-repl-*.tar.gz \
  --repo joshrotenberg/mcp-repl
```

Container provenance and SBOM attestations are attached to the GHCR image by
BuildKit and can be inspected with `docker buildx imagetools inspect`.

## Failure recovery

If native publication needs to be restarted, start a fresh `Release` workflow
**from main**, passing the existing tag; it builds that exact source using the
current publication checks. Do not dispatch from an old tag or rerun historical
workflow versions to recover a release: GitHub retains their original code,
which cannot acquire guards through a later commit. Preventing those historical
executions would require repository controls beyond these workflow changes.
If the crate was published but a new
source change is required, make the fix normally and let release-plz prepare a
new patch release. Do not move or overwrite an existing release tag.
