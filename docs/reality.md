# REALITY: which core speaks it, and the patched Mihomo

Users could not connect to some VLESS+REALITY servers:

- **Xray 26.9.8+ servers** (XTLS/REALITY `8cdf7bf`, "reject outdated/strange Client Hello that
  doesn't have X25519MLKEM768 before optional X25519") refuse a ClientHello without the hybrid
  `X25519MLKEM768` key share. **Xray 24.x servers** silently drop a hello whose REALITY key is the
  hybrid share. 25.x and 26.4 accept both, and no server says which generation it is.
- **Servers with `minClientVer`** compare the client version carried in the REALITY session id
  with their threshold and hand an older client to the cover site: the user sees a timeout.

## How REALITY traffic flows in MULTICORE

| Mode | Who speaks VLESS/REALITY | Affected before this change |
|---|---|---|
| Combined subscription (`multicore-json-massive`, or `multicore-mihomo` + `multicore-xray`): Mihomo routes, its proxies are loopback SOCKS bridges into Xray inbounds | **Xray v26.9.30** (`transport/internet/reality`) | No (see below) |
| Mihomo-only subscription (a single `multicore-mihomo` YAML, e.g. Remnawave Mihomo templates; `vless` proxies with `reality-opts`) | **Mihomo v1.19.32** (`component/tls/reality.go`) | **Yes**: both failures |
| A REALITY proxy written directly into the Mihomo half of a combined subscription | Mihomo | Yes, same as Mihomo-only |

`crates/multicore-core` passes both halves through unchanged apart from the documented
security rewrites (`snapshot.rs::stage_runtime_with_mihomo_control`, and in service mode
`config_sanitize.rs`), so the core that terminates the proxy decides the ClientHello.

### Xray (normal mode): not affected, not patched

Xray v26.9.30's REALITY client (`transport/internet/reality/reality.go`, `UClient`):

- writes its own version into the session id (`core.Version_x/y/z` = **26.9.30**), so it passes
  any `minClientVer` up to 26.9.30;
- sends the key shares of the chosen uTLS fingerprint unchanged (refraction-networking/utls
  `v1.8.3-0.20260301010127`). The presets `chrome`, `firefox` (148), `safari` (26.3), `ios`
  and `edge` mirror current browsers: `X25519MLKEM768` first, then `X25519`;
- derives the REALITY key from the standalone `X25519` share when there is one
  (`KeyShareKeys.Ecdhe`), else from the X25519 half of the hybrid share.

So a 26.9.8+ server sees the hybrid share first and accepts it, and a 24.x server (which only
knows `X25519`) authenticates the standalone share. The limits are the ones every Xray client
has and are server policy, not a client bug: a fingerprint without any hybrid share (legacy or
randomized presets) cannot reach a 26.9.8+ server, and a server with `maxClientVer` below
26.9.30 rejects it. MULTICORE does not patch Xray. `scripts/reality-lab.py --fetch-xray`
measures this against real servers (see Lab).

### Mihomo (Mihomo-only mode): affected, patched

Stock Mihomo v1.19.32 writes the fixed client version **1.8.2** and removes the hybrid share
from every hello unless a proxy sets `support-x25519mlkem768: true` (which then fails against
24.x servers, and only chrome can carry it). MetaCubeX treats both as out of scope
(MetaCubeX/mihomo#3193).

## The patched Mihomo

MULTICORE ships Mihomo v1.19.32 (commit `88dcbf7f`) plus the three patches of SlothClash PR #81,
vendored byte for byte in [`packaging/mihomo-patches/`](../packaging/mihomo-patches) (attribution
and GPL notice in its README):

| Patch | Effect |
|---|---|
| 0001 | client version in the session id is `26.9.9` instead of `1.8.2` (configurable) |
| 0002 | adaptive key share (`auto`): classic first, flip after one failure, pin after one success, flip again after three failures of a pinned verdict; learned per server address + public key. A fingerprint that cannot carry the hybrid share uses chrome for that handshake. `support-x25519mlkem768: true` still forces the hybrid share |
| 0003 | top-level config keys `reality-mlkem: auto\|on\|off` and `reality-client-version: "x.y.z"`; absent = `auto` and the core default |

The core logs its verdicts (`REALITY 203.0.113.7:443: X25519MLKEM768=true confirmed`), visible in
the Logs screen.

### Config keys

Both keys are **top-level** (not per-proxy `reality-opts`). MULTICORE writes neither, so the
default is the adaptive behaviour. A provider may set them:

- `snapshot.rs` (daemon staging) and `config_sanitize.rs` (service mode, which otherwise keeps
  only `ALLOWED_MIHOMO_KEYS`) keep them through `normalize_reality_policy`;
- a value the patched core would refuse (it fails the whole config over it: `reality-mlkem:
  always`, `reality-client-version: 1.9`) is dropped instead, so the core falls back to its
  defaults rather than refusing to start. Values are normalised to plain strings (`OFF ` -> `off`).

There is no Settings toggle: the default works for every server generation, and SlothClash's
toggle exists mainly for its separate-process reload flow.

## Building the core

[`packaging/mihomo-patches/manifest.json`](../packaging/mihomo-patches/manifest.json) pins:
the upstream source archive (URL + SHA-256, the same archive the notices name), every patch
(SHA-256, exact series), the Go toolchain (**go1.26.8**, the release MetaCubeX built v1.19.32
with; stock go.dev archives with their SHA-256 for Linux and Windows build hosts), the build
flags, and the SHA-256 of every output binary. `packaging/windows-*/versions.json` and
`packaging/linux/versions.json` pin the same binary hash per flavour (`components.mihomo.build`),
and `scripts/package-windows-release.ps1` carries it in its immutable pin tuple.

| Flavour | Target | CPU level | File |
|---|---|---|---|
| Windows x64 | `windows/amd64` | `GOAMD64=v1` (as the `amd64-compatible` build shipped before) | `mihomo-windows-amd64.exe` |
| Windows x86 | `windows/386` | `GO386=sse2` (as `windows-386`) | `mihomo-windows-386.exe` |
| Windows arm64 | `windows/arm64` | `GOARM64=v8.0` | `mihomo-windows-arm64.exe` |
| Linux deb/rpm/Arch/AppImage | `linux/amd64` | `GOAMD64=v1` (as `linux-amd64-compatible`) | `mihomo-linux-amd64` |

Flags are MetaCubeX's release flags: `CGO_ENABLED=0 -tags with_gvisor -trimpath -ldflags
"-extldflags --static -X constant.Version=v1.19.32 -X constant.BuildTime=<…> -w -s -buildid="`.
Differences from the stock MetaCubeX binaries, all deliberate:

- **Toolchain**: stock Go 1.26.8 instead of MetaCubeX's Go fork (whose patches keep Windows 7 /
  old macOS working). MULTICORE itself needs Windows 10+, so nothing is lost; `constant.Version`
  stays `v1.19.32`.
- **BuildTime**: the upstream commit time (`Wed Sep 30 14:35:49 UTC 2026`), not the wall clock.
- **CA bundle**: the one committed at the tag (MetaCubeX's CI replaces it with the runner's
  bundle at build time, which is not reproducible).

Every input is pinned and every target is cross-compiled with CGO disabled, so the binaries are
byte-identical on any host: two clean Windows builds produced the same hashes, and
`build-mihomo.sh` refuses (deleting the file) any binary whose hash differs from the manifest.

```bash
./scripts/build-mihomo.sh --output dist/mihomo [target ...]   # Git Bash or Linux
./scripts/build-mihomo-in-docker.sh --output dist/mihomo      # in the Linux build image
./scripts/test-build-mihomo.sh [--full]                       # tampered/extra/missing/stale patch, wrong pin
```

Output: the binaries, `SHA256SUMS`, `BUILDINFO.txt` and `mihomo-source.zip`.

### Releases

- **Windows** (`release.yml`, step "Build patched Mihomo", or the manual release scripts): run
  `build-mihomo.sh --output dist/mihomo windows-amd64 windows-386 windows-arm64` (Git Bash), then
  package with `MULTICORE_MIHOMO_BINARIES_DIR` (or `-MihomoBinariesDirectory`) pointing at it.
  The packager refuses to start without it, refuses a binary whose hash differs, refuses a
  production manifest that pins a stock Mihomo asset, and checks the recipe and the patches.
- **Linux** (`package-linux-release.sh`, run by `package-linux-in-docker.sh`): builds
  `linux-amd64` with `build-mihomo.sh` inside the build container (Go, sources and modules cached
  under the download cache), or takes `MULTICORE_MIHOMO_BINARIES_DIR`; the hash must match.
  `test-linux-package-contract.sh` checks the staged binary against the pin.
- **GPL**: the release's `mihomo-source.zip` is the `mihomo-source.zip` written by
  `build-mihomo.sh`: the complete patched tree that was compiled, with the patches, the manifest,
  the patch README and the build script under `MULTICORE-BUILD/`. The unpatched upstream archive
  is no longer attached as "the" Mihomo source. `THIRD_PARTY_NOTICES.md` names the modification.

### Bumping Mihomo or the series

1. Update `upstream` (version, commit, source URL/SHA-256/rootPrefix) and the Go pin in the
   manifest, rebase or drop patches (`git apply --3way` on the new tree, regenerate with
   `git diff`), update patch hashes, bump `series`.
2. `./scripts/build-mihomo.sh --output <dir> --update-pins` writes the new binary hashes into the
   manifest. Copy them into the four `versions.json` files and the packager's
   `Assert-ExactMihomoPin` tuple (`test-package-windows-release.ps1` checks they all agree).
3. Run the lab against the new core before releasing.

## Lab

`scripts/reality-lab.py` (a minimal port of SlothClash's `tests/reality-lab`) starts Xray
24.12.31 and 26.9.9 servers in Docker, each plain and with `minClientVer: 25.1.1`, and probes
every server with `chrome`, `chrome` + `support-x25519mlkem768` and `firefox` through each core
under test. Every cell has an expected result from the server-generation model; any mismatch
fails the run.

```bash
./scripts/build-mihomo-in-docker.sh --output out/mihomo linux-amd64
python3 scripts/reality-lab.py --patched out/mihomo/mihomo-linux-amd64 --fetch-stock --fetch-xray
```

Expected: patched `auto` passes every cell; patched `off` fails 26.9.9; patched `on` fails
24.12.31; stock fails every `minClientVer` server and 26.9.9 (except chrome with
`support-x25519mlkem768`); Xray 26.9.30 is expected to pass every cell (`chrome`, `firefox`).

Status: written and syntax-checked, **not yet run** (it needs Docker; run it on the Linux build
host before the release that ships this core).
