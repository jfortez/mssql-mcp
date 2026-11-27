# Release the crate

**Prepare release** opens a pull request for review. **Release** publishes to crates.io only when run manually after the release PR is merged.

## Prepare the release PR

1. In **Settings → Actions → General**, enable **Allow GitHub Actions to create and approve pull requests**. The workflow only creates PRs. It needs neither a personal access token nor a crates.io secret.
2. Run **Actions → Prepare release** from the default branch. For the first release, leave `version` blank to use `0.1.0` from `Cargo.toml`. For later releases, enter the new `MAJOR.MINOR.PATCH` version.
3. Review and merge the PR from `release/v<version>`. It contains `releases/v<version>.md` and, if the version changed, updates to `Cargo.toml` and `Cargo.lock`. If GitHub pauses CI for the bot PR, click **Approve workflows to run**.

Preparation runs formatting checks, tests, Clippy and `cargo package`. It never publishes or tags.

## Publish after the merge

1. Run **Actions → Release** from the default branch with `dry_run` enabled. It needs no registry token and creates no tag.
2. Configure the `CARGO_REGISTRY_TOKEN` Actions secret, then run **Release** again with `dry_run` disabled.

The workflow requires the reviewed notes and refuses an existing remote tag. After uploading to crates.io, it pushes the annotated tag and creates the GitHub Release from the reviewed notes.

## Recover a partial run

- **Branch pushed, PR not opened:** Rerun **Prepare release**. It reuses the branch only if its commit matches the prepared changes and has the current default-branch commit as its parent. It never overwrites a conflicting branch.
- **Upload succeeded, tag or GitHub Release failed:** Use the run's commit SHA to restore the missing tag or release. Verify any existing tag without moving it. Don't republish the same version.
