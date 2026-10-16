# Releasing Muxin

A release is a version tag (`v1.2.3`). Pushing one runs the **release** workflow
(`.github/workflows/release.yml`), which builds every platform and puts the
files in a **draft** GitHub release on this repository to look over and publish.

| File | What it is |
|---|---|
| `Muxin-<version>-macos.dmg` | The app for Apple silicon and Intel Macs (macOS 12 or later), signed with the Developer ID and notarized |
| `Muxin-<version>-linux-x86_64.tar.gz` | The app for Linux on Intel and AMD (glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora 36 and later), with `install.sh` |
| `Muxin-<version>-linux-aarch64.tar.gz` | The same for Linux on Arm |
| `Muxin-<version>-linux-<arch>.deb` | Debian and Ubuntu packages (x86_64 and aarch64) |
| `Muxin-<version>-linux-<arch>.rpm` | Fedora, openSUSE and RHEL 10 packages |
| `Muxin-<version>-linux-<arch>.pkg.tar.zst` | Arch packages (`pacman -U`) |
| `Muxin-<version>-linux-<arch>.AppImage` | The app for any distribution, in one file |
| `Muxin-<version>-windows-x64-setup.exe` | The Windows installer (for the user, no administrator needed) |
| `Muxin-<version>-windows-x64.zip` | The same, unpacked |
| `Muxin-<version>-windows-arm64-setup.exe` | The installer for Windows on Arm (which runs the x64 one too, emulated) |
| `Muxin-<version>-windows-arm64.zip` | The same, unpacked |
| `SHA256SUMS.txt` | Their checksums |

Each one carries the agent for every platform (`agents/`), which the app
installs on the machines it connects to. The Arm builds are made on GitHub's
Arm machines (`ubuntu-22.04-arm`, `windows-11-arm`), and each Linux and Windows
build runs its `muxin --version` and agent there before it's kept. The Linux
packages come from `scripts/linux-packages.sh` (nfpm and appimagetool, pinned
by checksum), and each one is installed on a fresh system first: the .deb on
the runner, the .rpm in Fedora, the Arch package in Arch (x86_64), and the
AppImage run as it is.

## Making a release

```sh
git switch main && git pull
scripts/release.sh 0.2.0 --push
```

The script sets the version in `Cargo.toml` and `Cargo.lock`, commits it, tags
the commit `v0.2.0` and pushes both. Leave out `--push` to look first; then
`git push origin HEAD v0.2.0`.

You can also run it on a branch that `main` follows, like `muxin`. Then `--push`
moves `main` up to it too, here and on GitHub. If `main` has commits the branch
doesn't, it stays where it is and the script says so.

Then:

```sh
gh run watch                                  # the build: about 30 minutes
gh release view v0.2.0                        # the draft and its files
gh release edit v0.2.0 --draft=false          # publish it
scripts/homebrew.sh 0.2.0 --push              # then the Homebrew cask
scripts/aur.sh 0.2.0 --push                   # and the AUR package
```

If a job fails for a reason outside the code, like a dropped connection to
Apple's servers, run just the failed jobs again: `gh run rerun <run id> --failed`.
The draft is made once every platform has built.

Published releases are public: anyone can download them from the repository's
Releases page.

## Homebrew

`brew install --cask lumarylabsllc/tap/muxin` installs Muxin on macOS and Linux
from [LumaryLabsLLC/homebrew-tap](https://github.com/LumaryLabsLLC/homebrew-tap).
Its cask, `Casks/muxin.rb`, points at a release's files by their checksums, so
it moves up only once the release is published: `scripts/homebrew.sh <version>
--push` writes it from the release's `SHA256SUMS.txt`, commits and pushes it.
`brew upgrade --cask muxin` gets it from then on.

## The AUR

`yay -S muxin-bin` installs Muxin on Arch from the
[AUR](https://aur.archlinux.org/packages/muxin-bin): a PKGBUILD that unpacks a
release's Linux tarball into `/usr/lib/muxin`, like the release's own Arch
package. `scripts/aur.sh <version> --push` writes the PKGBUILD and `.SRCINFO`
from the release's `SHA256SUMS.txt`, commits and pushes them with your AUR ssh
key, once the release is published.

To try the pipeline without a release, run the workflow by hand (Actions ›
release › Run workflow, or `gh workflow run release`). It builds the same files
and keeps them on the run as artifacts.

## Signing and notarization (once)

Without these secrets the workflow still builds everything, but the macOS app
is an **unsigned test build** (macOS refuses to open it until you allow it in
System Settings › Privacy & Security), and the draft says so.

**1. The Developer ID certificate.** In Keychain Access, under login › My
Certificates, right-click **Developer ID Application: Lumary Labs LLC
(WYY7PK57DM)** › Export, as a `.p12` with a password. Then:

```sh
base64 -i DeveloperID.p12 | gh secret set MACOS_CERT_P12 -R LumaryLabsLLC/muxin
gh secret set MACOS_CERT_PASSWORD -R LumaryLabsLLC/muxin     # asks for the password
rm DeveloperID.p12
```

**2. An App Store Connect API key, for the notary service.** In
[App Store Connect](https://appstoreconnect.apple.com/access/integrations/api)
› Users and Access › Integrations › Team Keys, generate a key with the
**Developer** role and download `AuthKey_<id>.p8` (Apple lets you download it
once). Note its **Key ID** and the **Issuer ID** above the list. Then:

```sh
gh secret set APPLE_API_KEY_P8 -R LumaryLabsLLC/muxin < AuthKey_<id>.p8
gh secret set APPLE_API_KEY_ID -R LumaryLabsLLC/muxin --body <key id>
gh secret set APPLE_API_ISSUER_ID -R LumaryLabsLLC/muxin --body <issuer id>
```

**3. On your Mac too** (to notarize a local build), save the same key for
`notarytool`:

```sh
xcrun notarytool store-credentials muxen --key AuthKey_<id>.p8 --key-id <key id> --issuer <issuer id>
```

Keep the `.p8` somewhere safe, like a password manager: Apple won't show it
again.

## Building by hand

| Platform | Command | Output |
|---|---|---|
| macOS | `scripts/bundle-macos.sh` | `dist/Muxin.app`, for this Mac (what you use day to day) |
| macOS release | `scripts/bundle-macos.sh --release && scripts/notarize-macos.sh` | a universal, notarized `dist/Muxin-<version>-macos.dmg` |
| Agents | `scripts/build-agents.sh` | `dist/agents/`, every platform (needs zig and cargo-zigbuild) |
| Linux | `scripts/package-linux.sh` | `dist/Muxin-<version>-linux-<arch>.tar.gz` |
| Windows | `scripts\package-windows.ps1` | the installer and zip (agents from a Mac or Linux first) |

## Not done yet

- **Windows code signing**: the installer isn't signed, so SmartScreen warns
  ("Windows protected your PC" › More info › Run anyway) until it is.
- **Updates from inside the app.**

## CI

`.github/workflows/ci.yml` runs on every push to `main` and every pull request:
the tests on macOS and Linux, and a Windows build. macOS minutes count ten
times against a private repository's Actions minutes, Windows twice.
