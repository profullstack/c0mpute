# Security

## Reporting a vulnerability

Please report security issues privately through
[GitHub security advisories](https://github.com/profullstack/c0mpute/security/advisories/new)
rather than a public issue.

## Release signing

Every artifact on a [c0mpute release](https://github.com/profullstack/c0mpute/releases)
(`c0mpute-<os>-<arch>.tar.gz`, `c0mpute-windows-x86_64.zip`) has a
[minisign](https://jedisct1.github.io/minisign/) signature next to it, `<artifact>.minisig`.

Public key ([`minisign.pub`](minisign.pub), key id `01A1B54137EC392A`):

```
RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM
```

The signed (trusted) comment reads `timestamp:<unix> version:<tag> file:<artifact>`, so a
signature only vouches for that one file of that one release.

What checks it:

- **`scripts/install.sh`** (served at `https://c0mpute.com/install.sh`) downloads the
  artifact and its `.minisig`, verifies the signature with the embedded key and checks the
  trusted comment names the artifact (and the release tag when it is known). It aborts on a
  missing signature, a failed verification or a mismatched comment. If `minisign` is not
  installed it downloads the pinned upstream minisign 0.12 release and checks it against a
  hard-coded sha256 before running it (Linux x86_64/aarch64, macOS arm64; on Intel macs
  install minisign with `brew install minisign`).
- **`c0mpute` self-update** (`node/crates/c0mpute-update`) verifies the same signature with
  the key compiled into the binary, after the sha256 check and before swapping the binary.
  No signature or a bad one means no update.
- **The release workflow** (`.github/workflows/release.yml`) signs in its `publish` job and
  refuses to publish a release that is missing any `.minisig`.

Verify a download by hand:

```sh
minisign -Vm c0mpute-linux-x86_64.tar.gz -P RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM
```

The installer script itself is fetched over HTTPS from c0mpute.com; read it before piping it
to `sh` if you prefer (`curl -fsSL https://c0mpute.com/install.sh | less`).
