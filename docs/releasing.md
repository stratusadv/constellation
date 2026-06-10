# Releasing constellation

How a new version ships to GitHub and to the Windows Package Manager (WinGet).

The pipeline is driven by `.github/workflows/release.yml`. Creating a GitHub release
builds the Windows binary, wraps it in an Inno Setup installer, attaches both to the
release, and (once enabled) opens a version-bump pull request against
`microsoft/winget-pkgs` so `winget upgrade` sees the new version.

## The short version (steady state)

Once the package already exists in WinGet and the repository secrets are in place, a
release is three steps:

1. Bump `version` under `[workspace.package]` in `Cargo.toml`, commit, push `main`.
2. Cut the release, tagged `v<version>` (the `v` is required, the rest must equal the
   `Cargo.toml` version):

        gh release create v0.1.1 --target main --title "v0.1.1" --notes "..."

3. Watch the `Release` workflow go green. It builds, attaches the installer, and opens
   the WinGet bump PR automatically.

Everything below is the detail behind those steps, and the one-time setup that has to
happen before the automation works.

## Package facts

| Field | Value |
|---|---|
| PackageIdentifier | `stratusadv.constellation` |
| Publisher | Stratus Advanced Technologies |
| Installer | Inno Setup, per-user (`Scope: user`) |
| Installer source | `assets/installer.iss` |
| WinGet community repo | `microsoft/winget-pkgs` |

The installer is per-user because `assets/installer.iss` sets `PrivilegesRequired=lowest`
and writes the PATH entry to `HKCU`. The WinGet manifest must therefore declare
`Scope: user`, or validation flags a mismatch.

## What the workflow does

Trigger: a GitHub release is **created** (`on: release: types: [created]`). Publish the
release, do not leave it as a draft, or the job will not see a public download URL.

`build-windows` job:

1. Verifies the release tag matches `Cargo.toml`. Tag `v0.1.1` is compared against the
   `[workspace.package] version`. A mismatch fails the build immediately, so bump the
   version and the tag together.
2. Builds `constellation.exe` in release mode.
3. Builds the installer from `assets/installer.iss` with Inno Setup, stamping the version.
4. Optionally signs it (see Signing below).
5. Attaches `constellation-setup-<version>.exe` and `constellation.exe` to the release.

`publish-winget` job (only when the repository variable `PUBLISH_WINGET` is `true`):

1. Downloads `wingetcreate`.
2. Runs `wingetcreate update stratusadv.constellation` pointed at the installer URL on the
   release, and submits the bump PR using the `WINGET_TOKEN` secret.

Note the verb: `update`. It bumps a package that is **already** in `winget-pkgs`. It cannot
create the package the first time. The very first submission is manual (see below), and
`PUBLISH_WINGET` stays unset until that first PR is merged.

## One-time setup

### Repository secrets and variables

Settings, then Secrets and variables, then Actions.

| Name | Kind | Purpose |
|---|---|---|
| `WINGET_TOKEN` | secret | GitHub classic PAT, `public_repo` scope. Lets `wingetcreate` fork `winget-pkgs` and open the bump PR. |
| `PUBLISH_WINGET` | variable | Set to `true` to enable the automatic WinGet job. Leave unset until the package exists in WinGet. |

The token must be a **classic** PAT with only `public_repo`. That scope is enough: it forks
the public `winget-pkgs`, pushes a branch, and opens a PR, all under your own account. Do
not grant full `repo`. Give the token a long or no expiration, or the automation breaks the
day it lapses. After a release runs once, `wingetcreate` caches the token in Windows
Credential Manager for local use.

### Public download URL

WinGet's validation bot and every `winget install` fetch the installer URL anonymously.
Release assets on a private repository are behind auth and will not resolve, so the
manifest is rejected. Either keep the repository public, or host the installer at a public URL
(a dedicated public releases repo, or a bucket or CDN) and point the manifest there. The
WinGet PR is public regardless: the installer URL, package name, publisher, and version
all become public the moment it is submitted.

## First submission to WinGet (one time per package)

Because the workflow only knows how to `update`, the package has to be created by hand
once. Do this after the first GitHub release exists, so the installer URL is live.

1. Create the classic PAT (`public_repo`) described above.
2. Install the tool locally:

        winget install Microsoft.WingetCreate

3. Generate the manifest from the released installer:

        wingetcreate new https://github.com/stratusadv/constellation/releases/download/v0.1.0/constellation-setup-0.1.0.exe

   Answer the prompts: PackageIdentifier `stratusadv.constellation`, version, Publisher
   `Stratus Advanced Technologies`, PackageName `constellation`, License `MIT License`.
   When asked about optional installer fields, set **Scope** to `user`. Accept the detected
   `inno` installer type and silent switches.
4. Submit with the token on the command line rather than the interactive browser login:

        wingetcreate submit -t <PAT> manifests\s\stratusadv\constellation\<version>

   The path is the version folder that `wingetcreate` saved (the `s` is the first letter of the
   identifier, which is how `winget-pkgs` shards manifests). Passing `-t` avoids the
   `Winget-Create` OAuth app, which asks for full public and private repository access. The
   `public_repo` PAT is the narrower path, take it.
5. The submit forks `winget-pkgs` and opens a PR.

### Signing the CLA

First-time contributors get a bot comment asking to sign Microsoft's Contributor License
Agreement. It grants Microsoft a license to host and redistribute the **manifest** you
submit (not constellation itself) and asks you to confirm you may give it. Because the tool
is published as the company's, reply on the PR with the company form:

    @microsoft-github-policy-service agree company="Stratus Advanced Technologies"

The agreement is standing: it covers all future submissions, including the automated
bump PRs, so it is signed only once.

### After submitting

1. Azure Pipelines validates the manifest schema, then runs the installer in a Windows
   Sandbox to confirm it installs silently. Watch the PR labels.
2. If a label like `Needs-Author-Feedback` or `Validation-Installation-Error` appears, read
   the bot comment, fix the manifest, and push to the fork branch. The PR re-validates.
   An unsigned installer can occasionally draw a Defender false positive.
3. A new publisher and package gets human moderator review. This can take hours to days.
4. On merge, the WinGet source index refreshes within the hour. Confirm:

        winget install stratusadv.constellation

5. Now enable the automation: set `WINGET_TOKEN` and `PUBLISH_WINGET=true`. Every
   later release opens its bump PR on its own.

## Signing (optional)

The workflow signs the installer through SignPath only when the repository variable
`SIGNPATH_ORGANIZATION_ID` is set. Without it, the unsigned installer is attached to the
release and accepted by WinGet. To enable signing, set the SignPath secret
(`SIGNPATH_API_TOKEN`) and variables (`SIGNPATH_ORGANIZATION_ID`,
`SIGNPATH_PROJECT_SLUG`, `SIGNPATH_POLICY_SLUG`). Signing is not required to publish,
but an unsigned installer may show a SmartScreen prompt on first run.

## Troubleshooting

- **Build fails on the version check.** The tag does not match `Cargo.toml`. Tag must be
  `v<version>` where `<version>` equals `[workspace.package] version`.
- **The WinGet job did nothing.** `PUBLISH_WINGET` is not `true`, or this is the first
  release and the package does not exist yet (use the manual `wingetcreate new` flow).
- **Manifest validation rejects the installer URL.** The asset is not publicly reachable.
  The repository is private, or the URL is wrong. See Public download URL.
- **Scope mismatch warnings.** The manifest must declare `Scope: user` to match the
  per-user installer.
- **The OAuth screen wants private repo access.** That is the `Winget-Create` app login.
  Cancel it and submit with `wingetcreate ... -t <PAT>` instead.
