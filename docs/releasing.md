# Releasing LazyAPI

LazyAPI uses `cargo-dist` to build GitHub release archives for Apple Silicon and Intel macOS, generate a Homebrew formula, and publish that formula to `SirwanAfifi/homebrew-tap`.

## One-time Homebrew setup

1. Create the tap repository. Homebrew's recommended layout is a public GitHub repository named `homebrew-tap`:

   ```bash
   brew tap-new SirwanAfifi/tap
   gh repo create SirwanAfifi/homebrew-tap \
     --public \
     --source "$(brew --repository SirwanAfifi/tap)" \
     --push
   ```

2. Create a fine-grained GitHub personal access token limited to the `SirwanAfifi/homebrew-tap` repository with read and write access to repository contents.

3. Add that token to the `SirwanAfifi/lazyapi` repository as the `HOMEBREW_TAP_TOKEN` Actions secret:

   ```bash
   gh secret set HOMEBREW_TAP_TOKEN --repo SirwanAfifi/lazyapi
   ```

   The command prompts for the token without placing it in shell history.

The release workflow is generated from [dist-workspace.toml](../dist-workspace.toml). When changing the dist configuration or upgrading `cargo-dist`, regenerate it with:

```bash
dist generate
```

## Publish a release

1. Update the version in `Cargo.toml`, update `Cargo.lock`, and run the full checks:

   ```bash
   cargo check
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-targets --all-features
   ```

2. Commit and push the version change.

3. Tag the same semantic version and push the tag. For the first release:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

The tag starts the generated release workflow. Stable tags publish the formula; prerelease tags build a GitHub prerelease but do not replace the stable Homebrew formula.

## Verify Homebrew

After the release workflow succeeds:

```bash
brew update
brew install SirwanAfifi/tap/lazyapi
lazyapi --version
```

Homebrew automatically taps `SirwanAfifi/homebrew-tap` when the fully qualified install command is used. Future tagged releases update the formula in the same tap.
