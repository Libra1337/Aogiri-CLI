# Releasing LynShen CLI

LynShen CLI follows the same distribution shape as Codex CLI:

- GitHub Releases are the canonical native artifacts.
- npm is a convenience install channel for the native binaries.
- The public npm entrypoint is `@lynshen/cli`.

## Package layout

- `npm/cli`: meta package with the `lynshen` launcher. It lists the four platform packages as optional dependencies.
- `npm/cli-linux-x64`: Linux x64 native binary.
- `npm/cli-win32-x64`: Windows x64 native binary.
- `npm/cli-darwin-arm64`: macOS Apple silicon native binary.
- `npm/cli-darwin-x64`: macOS Intel native binary.

`.github/workflows/release.yml` publishes the Linux and Windows packages. `.github/workflows/release-macos.yml`
publishes the two macOS packages and then `@lynshen/cli`.

## One-time setup

1. The npm organization `lynshen` owns the scope `@lynshen`. It was created on 2026-11-18, and all five packages have
   been on npm since 0.8.7.
2. Store an npm granular access token as the `NPM_TOKEN` Actions secret of `LynShen-Team/Aogiri-CLI`. The first
   publish showed what it needs:
   - **Packages and scopes:** Read and write on the scope `@lynshen`. Giving the token access to the organization
     alone does not let it publish.
   - **Bypass 2FA** ticked. Without it npm asks for a one-time password, which CI can't give, and the publish fails
     with E403.
   - **Expiry:** npm write tokens last about 90 days at most. Note the date, and before it passes create a new token
     and replace the secret. An expired token fails the npm steps; the GitHub Release is still created.
   - npm will stop direct publishing with granular tokens in January 2027 (see "Next: trusted publishing" below).
3. Confirm the repository has permission to create releases with `GITHUB_TOKEN`.

Without `NPM_TOKEN` both workflows skip the npm steps and leave a warning on the run. They decide this from the
secret itself (`HAS_NPM_TOKEN`), not from `NODE_AUTH_TOKEN`: `actions/setup-node` fills an empty `NODE_AUTH_TOKEN`
with the placeholder `XXXXX-XXXXX-XXXXX-XXXXX`, so testing that variable never skipped anything.

## Release flow

1. Bump the version in `Cargo.toml`.
2. Trigger the release workflows with `release_version=X.Y.Z`, or create and push a git tag in the form `vX.Y.Z`.
3. "Release CLI" builds the Windows and Linux binaries and "Release CLI (macOS)" the two macOS ones, in parallel.
   Each packs its npm packages, adds its files to the GitHub Release, and publishes to npm when `NPM_TOKEN` is set.
4. The macOS workflow publishes `@lynshen/cli` last, once npm serves all four platform packages of this version.

### npm needs minutes to serve a new version

`npm publish` returns before npm serves the version ("Your package is being processed and may take a few minutes to
become available"). Until then `npm view <package>@<version>` fails with E404, and a package published for the
first time shows only a `0.0.0-stage` placeholder version. For 0.8.7 the platform packages took 2 to 8 minutes.
The step that publishes `@lynshen/cli` waits up to 20 minutes for them; it used to wait 5 and timed out.

If that step still times out, wait until `npm view @lynshen/cli-<platform>@X.Y.Z version` answers for all four
platforms, then re-run the failed jobs of "Release CLI (macOS)". Every publish step skips packages npm already
serves; that re-run is how `@lynshen/cli` 0.8.7 went out.

## Local verification

1. `cargo build --release --target x86_64-pc-windows-msvc`
2. Copy or build each target binary into `artifacts/<rust-target>/`.
3. `npm run release:sync-version`
4. `npm run release:stage-binaries -- --source-root artifacts`
5. `npm run release:pack`

The generated npm tarballs are written to `dist/npm/`.

## Next: trusted publishing (not set up yet)

npm's trusted publishing lets a workflow publish with a short-lived OIDC token instead of `NPM_TOKEN`. It removes
the 90-day token and the secret that could leak, and it keeps working after January 2027, when npm removes direct
publishing with granular tokens ([About access tokens](https://docs.npmjs.com/about-access-tokens/)). As of
2026-11-18 nothing below is done.

Workflow changes (one PR, safe to merge before the npm side is set up, because npm tries OIDC first and falls back
to `NODE_AUTH_TOKEN`):

- Add `id-token: write` to the `permissions` of the `release` and `release-macos` jobs, next to `contents: write`.
- Use Node 24 in `actions/setup-node`. Trusted publishing needs npm 11.5.1 or later and Node 22.14.0 or later;
  Node 20, used now, comes with npm 10.
- Once `NPM_TOKEN` is gone, `HAS_NPM_TOKEN` is always false, so base the publish guard on something else, for
  example `github.repository == 'LynShen-Team/Aogiri-CLI'`.
- Provenance comes with it: the repository and the packages are public, so npm attaches a provenance statement to
  each version without `--provenance`.

On npmjs.com, the owner adds a trusted publisher to each of the five packages (package → Settings → Trusted
publishing → GitHub Actions):

| Package | Workflow filename |
| --- | --- |
| `@lynshen/cli-linux-x64`, `@lynshen/cli-win32-x64` | `release.yml` |
| `@lynshen/cli`, `@lynshen/cli-darwin-arm64`, `@lynshen/cli-darwin-x64` | `release-macos.yml` |

- Organization or user `LynShen-Team`, repository `Aogiri-CLI`, environment empty. The fields are case-sensitive
  and npm does not check them when saved; a mismatch shows up as ENEEDAUTH at publish time.
- Under allowed actions, also allow `npm publish`. A trusted publisher created after 2026-09-03 allows only
  `npm stage publish` by default, which would make every release wait for a 2FA approval on npmjs.com.
- A new trusted publisher must publish successfully within 2 days or it expires, so add them just before a
  release.

After the first release that publishes through OIDC: set each package's Settings → Publishing access to "Require
two-factor authentication and disallow tokens", revoke the npm token, and delete the `NPM_TOKEN` secret.

Without OIDC, the other way past January 2027 is a stage-only token with `npm stage publish`, where the owner
approves every version with 2FA on npmjs.com.
