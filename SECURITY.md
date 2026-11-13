# Security

## Reporting a vulnerability

Please report security issues privately through
[GitHub security advisories](https://github.com/profullstack/c0mpute/security/advisories/new)
rather than a public issue.

## Release signing

Every artifact a [c0mpute release](https://github.com/profullstack/c0mpute/releases) is
installed from has a [minisign](https://jedisct1.github.io/minisign/) signature next to it,
`<artifact>.minisig`:

| Artifact | What it is |
| --- | --- |
| `c0mpute-<os>-<arch>.tar.gz`, `c0mpute-windows-x86_64.zip` | the `c0mpute` binary |
| `c0mpute-plugins.tar.gz` | every `plugins/<id>/install.sh` (and its `module.toml`) of that tag |
| `c0mpute-tui.tar.gz` | the terminal UI source (`apps/tui`: `src/`, `package.json`, `tsconfig.json`) |
| `c0mpute-desktop-*.{AppImage,deb,dmg,zip,exe}` | the desktop app builds |

Public key ([`minisign.pub`](minisign.pub), key id `01A1B54137EC392A`):

```
RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM
```

The signed (trusted) comment reads `timestamp:<unix> version:<tag> file:<artifact>`, so a
signature only vouches for that one file of that one release.

### What signs

**The release workflow** (`.github/workflows/release.yml`). The platform jobs build the
binaries and the `extras` job packs the two archives from the tagged tree
(`scripts/pack-release-extras.sh`; run it yourself to see exactly what ships). The `publish`
job signs all of them and refuses to publish a release that is missing any binary, either
archive, or any of their `.minisig` files. The desktop builds are made after the release is
public and are signed by the `desktop-sign` job a few minutes later; until then they have no
signature and the installer will not install them.

### What verifies

- **`scripts/install.sh`** (served at `https://c0mpute.com/install.sh`):
  - **The binary.** Downloads the artifact and its `.minisig`, verifies the signature with
    the embedded key and checks that the signed comment names the artifact (and the release
    tag when one was asked for). A missing signature, a failed verification or a mismatched
    comment aborts the install.
  - **Which release.** Everything else in the run comes from the release named in the
    binary's signed comment (or, when the binary is already installed, from that binary's own
    release), and each signature must be for that tag.
  - **Plugin installers** (coinpay, infernet, transcode) run from the verified
    `c0mpute-plugins.tar.gz`, and **the TUI** is installed from the verified
    `c0mpute-tui.tar.gz`. If an archive is on the release, a missing or bad signature aborts
    the install.
  - **The desktop AppImage** (`--desktop`) is verified the same way. No signature means it
    is not installed.
  - **Releases up to v0.3.64** predate these signed extras. Only for those, and only when
    the release really does not have the asset (HTTP 404), the installer falls back to the
    old downloads (`https://c0mpute.com/plugins/<id>/install.sh`, the GitHub source tarball,
    the AppImage's `.sha256`) and prints a warning that they are unverified. On any newer
    release a missing extra is an error, so hiding the asset cannot force the fallback.
  - **minisign.** If `minisign` is not installed the script downloads the pinned upstream
    minisign 0.12 release and checks it against a hard-coded sha256 before running it (Linux
    x86_64/aarch64, macOS arm64; on Intel macs install minisign with `brew install minisign`).
  - **bun** (the runtime the TUI needs; only installed with the TUI). The script downloads
    the pinned upstream bun 1.3.14 release zip for the OS/arch and checks it against a
    hard-coded sha256 before unpacking it. It no longer pipes `bun.sh/install` to a shell, and
    it no longer installs mise at all (a contributor tool, not needed on a node). A platform
    without a pinned hash gets a message and no bun.
- **`c0mpute` self-update** (`node/crates/c0mpute-update`) verifies the binary's signature
  with the key compiled into the binary, after the sha256 check and before swapping the
  binary. No signature or a bad one means no update.
- **`c0mpute tui`** and **`c0mpute plugin install <id>`** download `c0mpute-tui.tar.gz` /
  `c0mpute-plugins.tar.gz` of the running binary's own release and verify them with that same
  compiled-in key (file and release tag included) before anything is unpacked or run. There
  is no unverified fallback.

`C0MPUTE_ALLOW_UNSIGNED=1` makes the installer accept a missing signature. It never accepts
a bad one.

### Rollback: what `latest` does and does not promise

A signature proves an artifact is a real c0mpute release. It does not prove it is the newest
one: whoever answers for `https://c0mpute.com/releases/latest/` (a redirect to GitHub's
latest release) could serve an older, genuinely signed release with a known bug.

What the installer does about it, without trusting a second server: it reads the release
version out of the signed comment, prints it (`installing c0mpute v<x.y.z>`), and when you
did not ask for a specific version it refuses to install a release older than the `c0mpute`
already on the machine. `C0MPUTE_VERSION=v<x.y.z>` installs an older release on purpose.
Self-update likewise only moves to a higher version.

What that does not prevent:

- A **first install** has nothing to compare against, so it can still be handed an older
  signed release. Check the printed version against the
  [releases page](https://github.com/profullstack/c0mpute/releases) if that matters to you.
- Being **held at the current version** (served the release you already have, never the
  newer one).

### What is still not verified

- **`install.sh` itself** is fetched over HTTPS from c0mpute.com; read it before piping it
  to `sh` if you prefer (`curl -fsSL https://c0mpute.com/install.sh | less`).
- **What the plugin installers fetch.** The signed `plugins/coinpay/install.sh` and
  `plugins/infernet/install.sh` are thin wrappers that download and run the upstream
  installers (`https://coinpayportal.com/install.sh`, `https://infernetprotocol.com/install.sh`)
  over HTTPS; `plugins/threatcrush/install.sh` does the same for ThreatCrush. Those upstream
  scripts and what they install are not covered by the c0mpute signature. The transcode
  installer installs ffmpeg through your system package manager. The video installer pulls a
  container image pinned by digest, and builds from a clone of the repository's default
  branch if that pull fails.
- **The TUI's npm dependencies.** The TUI source is signed, but `bun install` resolves its
  dependencies (`@profullstack/hqtui` and what it pulls in) from the npm registry at install
  time, without a lockfile.
- **`c0mpute plugin install --unverified <https url>`** runs somebody else's script. It
  refuses without the flag and warns with it.
- **`C0MPUTE_TUI_REF=<branch>`** (a developer override) installs the TUI from a branch
  tarball, with a warning.
- **The desktop app's own updater** and the macOS/Windows builds' platform code signing
  (there are no certificates yet).

### PATH, not root

The installer puts `~/.c0mpute/bin` on your PATH through your shell's rc file and prints the
line. It does not symlink the binary into `/usr/local/bin` with sudo any more: that put a
program your account can rewrite on root's PATH. If an earlier install left such a symlink,
the installer tells you; remove it with `sudo rm /usr/local/bin/c0mpute
/usr/local/bin/git-remote-c0mpute`. (Run as root, the installer still links there, since root
then owns the binary.) It does still write `/etc/profile.d/c0mpute.sh`, using sudo when it is
available: one `export PATH="$HOME/.c0mpute/bin:$HOME/.local/bin:$PATH"` line, which for
every account names that account's own home directory.

### Verify a download by hand

```sh
minisign -Vm c0mpute-linux-x86_64.tar.gz -P RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM
```
