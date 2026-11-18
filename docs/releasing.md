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
2. The workflows publish to npm through trusted publishing (OIDC), which each package must trust on npmjs.com. See
   "Trusted publishing" below.
3. Confirm the repository has permission to create releases with `GITHUB_TOKEN`.

The npm steps run only in `LynShen-Team/Aogiri-CLI`; npm trusts no other repository.

### No npm token

Since 0.8.8 (2026-11-18) the workflows publish only through trusted publishing, and every package is set to "Require
two-factor authentication and disallow tokens". There is no `NPM_TOKEN` secret and no npm token to rotate. If the
OIDC exchange fails, the npm steps fail and the GitHub Release is still created: fix the package's trusted publisher
(below), then re-run the failed jobs.

0.8.7 was published with a granular token. It needed read and write on the scope `@lynshen` (organization access
alone does not let a token publish) and "Bypass 2FA" (without it npm asks for a one-time password CI can't give, and
the publish fails with E403). npm removes direct publishing with granular tokens in January 2027, so don't go back to
one.

## Release flow

1. Bump the version in `Cargo.toml`.
2. Trigger the release workflows with `release_version=X.Y.Z`, or create and push a git tag in the form `vX.Y.Z`.
3. "Release CLI" builds the Windows and Linux binaries and "Release CLI (macOS)" the two macOS ones, in parallel.
   Each packs its npm packages, adds its files to the GitHub Release, and publishes to npm.
4. The macOS workflow publishes `@lynshen/cli` last, once npm serves all four platform packages of this version.
5. Once npm serves the version, check that all five packages carry a provenance statement (see "Checking a
   release" below).

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

## Trusted publishing

npm's trusted publishing lets a workflow publish with a short-lived OIDC token instead of a stored npm token. It
removes the expiring token and the secret that could leak, and it keeps working after January 2027, when npm
removes direct publishing with granular tokens ([About access tokens](https://docs.npmjs.com/about-access-tokens/)).

### In this repository (done)

- The `release` and `release-macos` jobs have `id-token: write` next to `contents: write`.
- `actions/setup-node` installs Node 24. Trusted publishing needs npm 11.5.1 or later and Node 22.14.0 or later;
  Node 20 came with npm 10.
- Each `npm/*/package.json` has `repository` pointing at `https://github.com/LynShen-Team/Aogiri-CLI`. npm adds a
  provenance statement to every version published through OIDC from a public repository, and rejects the publish
  when `repository.url` doesn't match the repository the workflow ran in.
- `actions/setup-node@v6`: from v6.1 it no longer writes `always-auth` into `.npmrc`, which npm 11 warns about.
  `package-manager-cache: false` keeps release builds from restoring a cache. The token line setup-node writes into
  `.npmrc` holds a placeholder that OIDC publishing doesn't use.
- No `NODE_AUTH_TOKEN` and no npm secret.

### On npmjs.com (done 2026-11-18)

Each of the five packages has a trusted publisher, added on 2026-11-18 at about 15:30 UTC:

| Package | Workflow filename |
| --- | --- |
| `@lynshen/cli-linux-x64`, `@lynshen/cli-win32-x64` | `release.yml` |
| `@lynshen/cli`, `@lynshen/cli-darwin-arm64`, `@lynshen/cli-darwin-x64` | `release-macos.yml` |

All five use repository `LynShen-Team/Aogiri-CLI` and no environment. They allow `npm publish` as well as
`npm stage publish`; a trusted publisher created after 2026-09-03 allows only `npm stage publish` unless told
otherwise, which would make every release wait for a 2FA approval on npmjs.com.

0.8.8 went out through all five on 2026-11-18, which validated them. A new trusted publisher expires unless it
publishes successfully within 2 days, so add or replace one only just before a release.

Each package's Publishing access is "Require two-factor authentication and disallow tokens" (set on 2026-11-18 with
`npm access set mfa=publish <package>`, which asks for 2FA once per package). Trusted publishing is not affected.

#### Adding or replacing one

- The npm account must have two-factor authentication turned on (Account → Two-Factor Authentication). Without it
  `npm trust` fails with `403 Forbidden`; the registry's own message, `Please enable 2fa for your account`, is not
  printed. A token with "Bypass 2FA" can't change trusted publishers either.
- On the website: package → Settings → Trusted publishing → GitHub Actions. The fields are case-sensitive and npm
  does not check them when saved.
- From a terminal (npm 11.15.0 or later), after `npm login`:

  ```sh
  npm trust github @lynshen/cli --file release-macos.yml --repo LynShen-Team/Aogiri-CLI --allow-publish --yes
  ```

  The first such command asks for 2FA, and npm's page can skip 2FA for the next 5 minutes. `npm trust list <package>`
  shows what a package has. Run these in an interactive terminal, where npm can ask for 2FA. Run `npm logout` when
  done.
- A trusted publisher can't be edited. To replace one, remove it (`npm trust revoke --id <id> <package>`, the id
  from `npm trust list`, or delete it on the website) and add it again.

### Checking a release

Once npm serves the version:

```sh
for p in cli cli-linux-x64 cli-win32-x64 cli-darwin-arm64 cli-darwin-x64; do
  echo "@lynshen/$p: $(npm view "@lynshen/$p@X.Y.Z" dist.attestations.provenance.predicateType)"
done
```

Each line should end in `https://slsa.dev/provenance/v1`: npm adds that provenance statement to versions published
through OIDC. The npm steps in the release logs also say "Signed provenance statement with source and build
information from GitHub Actions". npm serves a version's metadata a little before its tarball: right after
0.8.8's `@lynshen/cli` was published, `npm install` got a 404 for the tarball for about a minute.
