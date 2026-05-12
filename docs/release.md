# Release setup

This project publishes the CLI crate to crates.io and attaches unsigned desktop app bundles to GitHub Releases.

## Create a crates.io token

1. Sign in to crates.io: https://crates.io/
2. Open account API tokens: https://crates.io/settings/tokens
3. Create a new token.
4. Give it publish access for the `toggl-jira-sync` crate.
5. Copy the token once. crates.io will not show it again.

Official docs: https://doc.rust-lang.org/cargo/reference/publishing.html#before-publishing-a-new-crate

## Add the token to GitHub

1. Open the repository on GitHub.
2. Go to `Settings` -> `Secrets and variables` -> `Actions`.
3. Click `New repository secret`.
4. Set the secret name to `CARGO_REGISTRY_TOKEN`.
5. Paste the crates.io token as the value.
6. Save the secret.

GitHub docs: https://docs.github.com/en/actions/security-guides/using-secrets-in-github-actions

## Publish a release

1. Bump the version in `Cargo.toml`.
2. Keep the Tauri app version in `tauri/src-tauri/tauri.conf.json` aligned when releasing desktop builds.
3. Commit the version bump.
4. Push a tag matching the crate version, for example `v0.1.9`.

The release workflow checks that the tag version matches the crate version, publishes the CLI crate to crates.io, builds unsigned desktop bundles, and creates a draft GitHub Release with the bundle assets attached.
