# Release setup

This project publishes the CLI crate to crates.io with trusted publishing and attaches unsigned desktop app bundles to GitHub Releases.

## Configure crates.io trusted publishing

1. Sign in to crates.io: https://crates.io/
2. Open the `toggl-jira-sync` crate settings on crates.io.
3. Add a trusted publisher for GitHub Actions.
4. Set repository owner to `gaboe`.
5. Set repository name to `toggle-jira-sync`.
6. Set workflow filename to `release.yml`.
7. Leave environment empty unless the GitHub workflow is changed to use one.

Official docs: https://crates.io/docs/trusted-publishing

## Publish a release

1. Bump the version in `Cargo.toml`.
2. Keep the Tauri app version in `tauri/src-tauri/tauri.conf.json` aligned when releasing desktop builds.
3. Commit the version bump.
4. Push a tag matching the crate version, for example `v0.1.16`.

The release workflow checks that the tag version matches the crate version, exchanges the GitHub Actions OIDC identity for a short-lived crates.io token, publishes the CLI crate to crates.io, builds unsigned desktop bundles, and creates a draft GitHub Release with the bundle assets attached.
