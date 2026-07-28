<div align="center">

# FSLABSCLI

## License

fslabscli is free and open source! All code in this repository is dual-licensed under either:

* MIT License ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

at your option. This means you can select the license you prefer! This dual-licensing approach is the de-facto standard in the Rust ecosystem and there are very good reasons to include both.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
</div>

## Installation

To install, run the following command:
``cargo install --git https://github.com/fslabs/fslabscli``

## Test annotations

`fslabscli rust-tests` posts the `file:line` locations of cargo failures as a
`test-annotations` GitHub check run, so they render inline on the pull request's
*Files changed* tab. The conclusion is always `neutral` — this reports
locations, not a verdict, and the CI job's own check remains the pass/fail gate.
Posting never fails the build: if it cannot post, it logs a warning saying why.

It needs to know which commit to annotate. Prow injects these into every
decorated job, so there is nothing to configure in CI:

| Variable | Required | Purpose |
|---|---|---|
| `REPO_OWNER` | yes | Repository owner |
| `REPO_NAME` | yes | Repository name |
| `PULL_PULL_SHA` | yes | Commit to attach the check run to; falls back to `PULL_BASE_SHA` |
| `PULL_NUMBER` | no | Links findings into the PR diff rather than the blob view |
| `FSLABSCLI_ANNOTATIONS_DISABLE` | no | Set to `1` or `true` to turn posting off |

It also needs a credential with the `checks: write` permission, from one of:

| Variable | Purpose |
|---|---|
| `GITHUB_TOKEN` | Used directly when present |
| `FSLABSCLI_CHECKS_APP_ID` + `FSLABSCLI_CHECKS_APP_PRIVATE_KEY` | App ID and private-key path; mints a token scoped to the repository under test |

Prefer the App in CI. A test job compiles and runs unreviewed pull-request code,
so any credential it holds should be assumed readable by that code; an
installation token scoped to one repository and holding only `checks: write` is
far less useful to an attacker than a broadly-scoped one. The private key must
belong to an app installed on the repository being tested.

## Release Process

**Version source of truth:** `Cargo.toml`
**Tag format:** `v{version}` (e.g., `v2.43.0`) — configurable via `--tag-format`

### Steps

1. **Bump version** — Update `version` in `Cargo.toml`, open a PR with appropriate labels, merge to `main`. PRs with `skip-changelog` are excluded from release notes. Changelog categories are controlled by [.github/release.yml](.github/release.yml).

2. **Draft release auto-created** — On push to `main`, a Prow postsubmit runs `fslabscli draft-release`, which:
   - Creates or updates a draft GitHub Release tagged `v{version}`
   - Generates changelog from merged PRs (GitHub native release notes)
   - Builds and uploads binary artifacts to the draft
   - Pushes the Nix build to the cache

3. **Publish the draft** — Review the draft release on GitHub, verify all assets are present, click **"Publish release"**. This creates the git tag and locks the release assets.

4. **Post-publish automation** — The new tag triggers Prow to:
   - Publish the crate to the `fsl` Cargo registry
   - Build and push the Docker image
   - Mark the release as **"latest"**
   - Kargo detects the new tag and starts canary promotion

### Flow

``` mermaid
sequenceDiagram
    actor Dev as Developer
    participant GH as GitHub (main)
    participant Prow as Prow (draft-release)
    participant Rel as GitHub Release
    participant Prow2 as Prow (post-publish)
    participant Kargo as Kargo

    Dev->>GH: Merge PR (version bump in Cargo.toml)
    GH->>Prow: Postsubmit on push to main
    Prow->>Prow: Build Nix binary, push to cache
    Prow->>Rel: Create/update draft release<br/>tag: v{version}, changelog from PR labels
    Prow->>Rel: Upload binary artifacts

    Dev->>Rel: Review draft, verify assets, click "Publish release"
    Note over Rel: Git tag created, assets locked

    Rel->>Prow2: Webhook (tag created)
    Prow2->>Prow2: Publish crate to fsl registry
    Prow2->>Prow2: Build & push Docker image
    Prow2->>Rel: Mark as "latest"

    Rel->>Kargo: Tag detected
    Kargo->>Kargo: Start canary promotion
```
