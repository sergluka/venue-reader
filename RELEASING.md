# Releasing

This guide is for maintainers preparing and publishing server versions. For
pull requests, use [Contributing](CONTRIBUTING.md).

## Tools and packaging

Use the pinned Rust toolchain and Cargo.lock. Install the release tools:

```sh
cargo install cargo-dist --version 0.33.1 --locked
cargo install cargo-about --version 0.9.3 --locked --features cli
cargo install cargo-release --version 1.1.7 --locked
cargo install git-cliff --version 2.14.2 --locked
```

Check that `dist --version` reports `cargo-dist 0.33.1`. Targets, runner labels,
archive formats and included notices are defined in `dist-workspace.toml`.
Platform baselines and installer checksum behavior are documented in the
[README](README.md#binary-installers).

After changes to dependencies, features or targets, regenerate notices:

```sh
cargo about generate --locked --fail about.hbs --output-file DEPENDENCY_LICENSES.txt
```

The runtime dependency notices and bundled FIX data attribution remain separate
in `DEPENDENCY_LICENSES.txt` and `THIRD_PARTY_LICENSES.txt`.

Check packaging changes locally:

```sh
dist generate
dist generate --check
dist plan --output-format=json
dist build --artifacts=local --target x86_64-unknown-linux-gnu
```

The archive and SHA-256 file are written to `target/distrib/`. Native CI checks
cover the other targets. Edit dist configuration and supported reusable workflows;
the generated `release.yml` must not be edited by hand.

## Prepare a version

Run the [contributor checks](CONTRIBUTING.md) and affected wiki checks. Review
the source revision and wait for its CI results before delivering a release tag.

Use a clean checkout containing the changes to release. Commit messages follow
Conventional Commits: `feat` entries become Added, `fix` entries become Fixed,
and `perf` entries become Performance. Breaking changes are retained for every
commit type; other maintenance entries are omitted. Preserve exact technical
identifiers in commit messages.

Preview a selected version:

```sh
cargo release 0.1.0
```

Dry-run is the default and changes no files or refs. To execute the release:

```sh
cargo release 0.1.0 --execute
```

Cargo-release updates Cargo.toml/Cargo.lock, generates CHANGELOG.md with git-cliff,
regenerates dependency licences, and creates the local release commit and tag.
The hook skips file generation during dry-run. Existing
prerelease tags are ignored as changelog boundaries so their changes appear in
the stable notes. A separate tag push starts GitHub Actions, which builds and
verifies the tagged source before creating the GitHub Release with archives,
installers and checksums.

`publish = false` and `push = false` keep release preparation local. Uploading
the branch and tag is a separate operation.
The generated changelog supplies cargo-dist's release notes. A failed hook stops
preparation before commit/tag. Keep native archive and installer checks in
CI before GitHub Release creation.

Push the branch and annotated tag when the candidate is ready:

```sh
git push --follow-tags origin HEAD
```

## Verify the release

The release pipeline checks source and package contents, then exercises the
four native archives and stock installers before announcement. It uploads the
complete asset set before publishing the immutable GitHub Release. After
announcement, it downloads each published archive and repeats the checksum,
extraction and LSP smoke checks. Review the complete workflow result and retain
the source SHA, package/archive checksums and tested platform results.

Native checks run on the configured CI runners; they do not prove execution on
every older OS allowed by the documented baseline. Keep immutable release bytes
and tags unchanged. Repairs to a published artifact require a new version.

## crates.io publication

`verify-release.yml` checks the published archives before calling the registry
publisher. Cargo-dist calls it after announcement; owner-published releases and
manual runs use the same workflow. The publisher checks out the exact release
tag and accepts only a published, immutable stable release whose version matches
Cargo.toml.

Repository variables control publication:

| Variable | Value |
| --- | --- |
| `CRATES_IO_PUBLISH` | `true` enables uploads; unset disables them. |
| `CRATES_IO_AUTH_MODE` | `bootstrap` for the first crate upload; `oidc` for Trusted Publishing. |

The repository must be public. Private repositories and prereleases skip the
publisher even when upload is enabled. The existing source-package gate builds
and verifies the crate before publication. The publisher uses
`cargo publish --locked --no-verify`, so that job does not repeat the build with
publication credentials.

For the first upload, verify the crates.io account email and create a GitHub
environment named `crates-io`. Store a short-lived crates.io token in that
environment's `CRATES_IO_TOKEN` secret. Limit it to `publish-new` and the crate
name `venue-reader`. Set `CRATES_IO_AUTH_MODE=bootstrap` and enable publication.

To publish an existing verified GitHub Release, run:

```sh
gh workflow run verify-release.yml -f tag=v0.1.0
```

Subsequent tag releases call the publisher automatically after their checks.
An existing crate version remains a Cargo error; the workflow does not silently
skip it. If an upload was interrupted, inspect the registry package and its
checksum before accepting publication. Retry against the same approved tag;
keep the GitHub Release and its immutable assets.

After the first upload, add two Trusted Publisher records in the crate's
crates.io settings, both for owner `sergluka`, repository `venue-reader` and
environment `crates-io`: one for `release.yml` and one for `verify-release.yml`.
These are the calling workflow identities; the reusable `publish-crate.yml`
is not a separate entry point. Set `CRATES_IO_AUTH_MODE=oidc`, revoke the
bootstrap token, and delete its GitHub secret. OIDC failure stops publication;
it does not fall back to the bootstrap token.

## Workflow permissions

Cargo-dist grants `contents: write` to its built-in release jobs. Custom source,
archive and installer checks use `contents: read`. The registry publisher needs
`id-token: write` at both caller and callee for OIDC. Archive verification keeps
read-only permissions even when called from the publishing workflow.

Built-in jobs' `GH_TOKEN` is visible to code they execute; disabling checkout
credential persistence does not remove that environment variable.
