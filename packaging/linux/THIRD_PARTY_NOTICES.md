# Third-party notices

This Linux package contains executable builds from two independent upstream
projects. MultiCore does not claim ownership of either project or its marks.

## Xray-core

- Project: XTLS/Xray-core
- Version/tag: v26.9.30
- Commit: `b26a91de4f3294e26a0ad0a970b81a386a41f789`
- License: Mozilla Public License 2.0 (included as
  `licenses/Xray-core-MPL-2.0.txt`)
- Pinned source snapshot:
  `https://github.com/XTLS/Xray-core/archive/b26a91de4f3294e26a0ad0a970b81a386a41f789.zip`
- Observed source snapshot SHA-256:
  `eb89ddcf9d2eb622efa6bb45c1f04ab8f2b52af81d891f05ba7e23ac57cd8a8b`

## mihomo

- Project: MetaCubeX/mihomo
- Version/tag: v1.19.32
- Commit: `88dcbf7f1614a67c3b36b848ee3592dfa92ada36`
- License: GNU General Public License version 3 only (included as
  `licenses/mihomo-GPL-3.0.txt`)
- Pinned source snapshot:
  `https://github.com/MetaCubeX/mihomo/archive/88dcbf7f1614a67c3b36b848ee3592dfa92ada36.zip`
- Observed source snapshot SHA-256:
  `63ad2b98fb7a1067422af350a172e2c656c45a2564d0202b5181c6dc4cc7dd30`

## Twemoji Mozilla font

- Project: Mozilla/twemoji-colr
- Release: v0.7.0
- Source: `https://github.com/mozilla/twemoji-colr`
- Font/code license: Apache License 2.0
- Emoji artwork license: CC BY 4.0
- Artwork attribution: Emoji artwork by the Twemoji project
- Font packaging attribution: Twemoji Mozilla COLR/CPAL font packaging by Mozilla
- License reference: `https://creativecommons.org/licenses/by/4.0/`

## Noto Emoji 3D images

- Project: googlefonts/noto-emoji (`3D/png/72`, commit `e20cbc2bbec1926686be9f9bee7d1d2cfa1fea0e`)
- Source: `https://github.com/googlefonts/noto-emoji`
- License: image resources Apache License 2.0; fonts SIL Open Font License 1.1
  (bundled as `assets/emoji/LICENSE`); flag images are public domain or
  otherwise exempt from copyright
- Use: every emoji rescaled to 40px WebP and embedded in the desktop app so
  colour emoji render without GPU font support

## Rust update-bundle libraries

- `sha2` by the RustCrypto Developers — MIT OR Apache-2.0
- `zip` by the zip-rs contributors — MIT

Their source and license metadata are recorded by Cargo.lock and the linked
upstream repositories. A public binary release still requires a complete
dependency/license inventory rather than relying only on this short notice.

The GitHub commit URLs and recorded hashes are provenance records, not a complete
public-release compliance process. Public redistribution requires immutable
corresponding-source artifacts that remain available through the required offer
period, plus a license-compliance and legal review appropriate to the release
channel. The current generator intentionally produces a local preview only.
