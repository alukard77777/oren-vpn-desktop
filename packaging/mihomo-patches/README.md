# Mihomo patch series

MULTICORE ships [Mihomo](https://github.com/MetaCubeX/mihomo) built from the pinned MetaCubeX
release source plus the patches in this directory. `scripts/build-mihomo.sh` applies them in
order and builds every bundled binary; `manifest.json` pins the source, each patch, the Go
toolchain and every resulting binary by SHA-256. Why the series exists and how to bump it:
[docs/reality.md](../../docs/reality.md).

| Patch | What it changes |
|---|---|
| `0001-reality-configurable-client-version.patch` | REALITY sends client version `26.9.9` instead of the fixed `1.8.2`, so Xray servers with `minClientVer` stop handing the connection to their cover site |
| `0002-reality-adaptive-mlkem.patch` | Adaptive `X25519MLKEM768` key share: learned per server, so both Xray 24.x (drops a hybrid hello) and Xray 26.9.8+ (requires it) connect; a fingerprint without a hybrid share uses chrome for that handshake |
| `0003-reality-config-keys.patch` | Top-level config keys `reality-mlkem: auto\|on\|off` and `reality-client-version: "x.y.z"` (absent = auto and the core default) |

## Attribution and licence

The three patches are taken unmodified (byte for byte; their SHA-256 values equal the ones in
SlothClash's `core/mihomo/manifest.json`) from **SlothClash** by Nemu-x,
<https://github.com/Nemu-x/SlothClash>, directory `core/patches/mihomo/` at commit
`9a09ac799e9c63ed170cbbc039670be65311afff` (pull request
[#81](https://github.com/Nemu-x/SlothClash/pull/81)). The file names are kept.

They modify Mihomo, Copyright (C) MetaCubeX and contributors, which is licensed under the GNU
General Public License version 3 (`licenses/mihomo-GPL-3.0.txt` in every MULTICORE package).
The patches, and the patched Mihomo binaries MULTICORE ships, are distributed under the same
GNU GPL version 3. This program is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR
PURPOSE. See the GNU General Public License for more details.

Every MULTICORE release attaches `mihomo-source.zip`: the complete patched source tree that was
compiled, with these patches, `manifest.json` and `build-mihomo.sh` under `MULTICORE-BUILD/`.
MULTICORE's own code is licensed under GPL-3.0-only as well; Mihomo runs as a separate program.
