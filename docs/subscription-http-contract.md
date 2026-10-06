# MultiCore subscription HTTP contract

This document is the backend-facing contract implemented by the current MultiCore client. Header names are case-insensitive. Subscription bodies are UTF-8 and are never share links, Base64 subscriptions, Clash JSON, or sing-box documents.

## Fetch sequence

For one subscription URL, the client first sends one request with `User-Agent: multicore-json-massive`.

- A successful HTTP status whose body is a JSON array is terminal: the client parses that bundle and does not make Mihomo/Xray fallback requests, even if the bundle is invalid.
- A successful status with any other body (an HTML page, share links, Base64, plain YAML), a network failure, or a non-success HTTP status triggers two parallel requests with `multicore-mihomo` and `multicore-xray`.
- When the combined request has not answered within 3 seconds, the two per-core requests start alongside it. A JSON-array bundle that still arrives first wins; a successful Mihomo answer that arrives first wins and the combined request is abandoned. A Mihomo YAML without a loopback SOCKS bridge into Xray (see below) waits at most 2 more seconds for the Xray answer. A panel that is slow to refuse requests it does not serve therefore costs seconds, not its gateway timeout.
- A leading UTF-8 byte order mark is ignored on every representation.
- Redirects are followed like a browser, up to five hops. An `https` to `http` downgrade, a non-HTTP scheme, and a redirect from a public host to loopback, private or `localhost` addresses are refused.
- While MULTICORE is connected in system-proxy mode, every request (subscription, service logo, group icons) goes through the connection first (Mihomo's loopback mixed port, 15 seconds per subscription request, 6 per image), then directly once if that fails. Image hosts are still checked to resolve to global addresses before either. In TUN mode direct requests already follow the tunnel's routes; while disconnected requests go directly.
- Each direct request is retried once: after a 429/500/502/503/504 that arrived within 5 seconds (honouring `Retry-After` up to 3 seconds; a slower one is a gateway timing out and is final), or through the operating-system proxy when a direct connection cannot be established. Each attempt has a 15-second connect and 30-second total timeout; the whole download is bounded by 45 seconds.
- Failures are reported to the user by cause (unreachable, timeout, TLS, redirect, HTTP 401/403, 404/410, 429, 5xx, not a MultiCore subscription, invalid configuration). Subscription URLs never appear in these messages or in diagnostics.
- Both fallback responses must succeed and validate before the client publishes anything, with one exception below (Mihomo-only subscriptions).
- If the Mihomo response carries `X-Hwid-Max-Devices-Reached: true` (Remnawave answers a device over its HWID limit with an empty `200` and this header), the refresh fails with a device-limit error instead of an invalid-config one.

### Mihomo-only subscriptions

When the Mihomo YAML contains no loopback SOCKS bridge into Xray (no `socks5` proxy on `127.0.0.1`), Xray carries no traffic. The Xray response is then optional: if it fails, is not 2xx, or is not one JSON object (share links, Base64, a Remnawave-style array of configs), the client keeps the Mihomo YAML and runs an idle Xray config instead (`{"inbounds":[],"outbounds":[{"tag":"multicore-idle","protocol":"freedom"}]}`). A valid provider Xray object is still used when present. As soon as the YAML bridges into Xray, the Xray response is required again. This lets panels such as Remnawave serve MultiCore with one `multicore-mihomo` response rule; see the Remnawave guide in the public release repository.
- A failed refresh preserves the complete previous last-good snapshot.

## Request headers

Every config request includes these identity headers:

| Header | Value |
| --- | --- |
| `x-hwid` | Stable uppercase UUID-shaped installation identifier. Derived through the INCY-compatible scheme when platform inputs are available (Windows `MachineGuid`; Linux `/etc/machine-id`), otherwise random, and persisted. |
| `x-device-os` | `windows` or `linux` |
| `x-ver-os` | Bounded OS version string: Windows product and release, or Linux `os-release` `PRETTY_NAME`. |
| `x-device-model` | Bounded device model, with `Windows PC` or `Linux PC` fallback. |

The client never sends raw MachineGuid, hostname, or user name.

### Combined request

```http
User-Agent: multicore-json-massive
Accept: application/vnd.multicore.bundle+json, application/json
```

Successful body: a JSON array of exactly two elements:

```json
[
  "<complete Mihomo YAML as a JSON string>",
  { "<complete Xray configuration>": "as one JSON object" }
]
```

The first decoded element must be the full Mihomo YAML string. The second element must be one JSON object, not a quoted JSON string or an array.

### Mihomo request

```http
User-Agent: multicore-mihomo
Accept: application/json, application/yaml, text/yaml, text/plain
```

Successful body: one complete Mihomo YAML mapping ready to run. Mihomo owns TUN, DNS, process routing, rules, and selectors.

### Xray request

```http
User-Agent: multicore-xray
Accept: application/json, application/yaml, text/yaml, text/plain
```

Successful body: one complete Xray JSON object. It contains the provider-defined FinalMask-capable outbounds and matching loopback SOCKS inbounds used by Mihomo.

### Service-logo request

When a valid logo URL is returned, the client performs a separate request:

```http
User-Agent: multicore-service-logo
Accept: image/png, image/jpeg, image/webp, image/svg+xml
```

Identity headers are deliberately omitted from logo requests. Redirects are disabled. Only HTTPS logo URLs resolving exclusively to global addresses are accepted; the result is bounded, decoded, normalized, and cached as PNG.

### Group icons

A Mihomo `proxy-groups` entry may name an image in `icon:` (an absolute HTTPS URL; PNG, JPEG, WebP or SVG). On each subscription refresh MultiCore downloads up to 32 distinct icon URLs with the same rules as the service logo (`multicore-group-icon` user agent, no identity headers, no redirects, global addresses only, 512 KiB source limit, 12 s per icon, 20 s for all icons together), normalizes each to a PNG of at most 128 px and keeps it with the subscription, so launching the app never refetches them. The icon replaces the group's emoji or fallback glyph in the group tabs, the tray and wherever that group appears as a route (for example an automatic group inside the main selector). An icon that fails to download keeps its previous copy; a missing or invalid icon falls back to the emoji or glyph. Emoji at the start of a group name keep working as before.

## Accepted response metadata

Metadata may be returned on the successful combined response. In fallback mode, fields are merged independently: the first valid Mihomo value wins, and only a missing/invalid field falls back to Xray.

| Purpose | Accepted response headers | Format and behavior |
| --- | --- | --- |
| Display title | `profile-title`, then any non-empty vendor-prefixed `*-servicename`, then `Content-Disposition: ...; filename=...` | Plain UTF-8 or `base64:` for the first two. Standard/URL-safe Base64 with optional padding. Sanitized, maximum 128 characters; credential-like values are rejected. Source host is the final fallback. |
| Usage and expiry | `subscription-userinfo` or any name ending in `-subscription-userinfo` | Semicolon fields `upload=<u64>; download=<u64>; total=<u64>; expire=<u64>`. Byte counters use bytes; `expire` is Unix seconds. `total=0` or `expire=0` means unspecified/unlimited. |
| Refresh interval | `profile-update-interval` | Integer hours. Converted to seconds and clamped from 15 minutes through 30 days. While the active profile is ready or connected, the desktop uses this interval for automatic refresh. If omitted, the client uses six hours. Failed automatic attempts back off for 15 minutes. Connected refreshes fetch through the TUN and defer routing-config activation until reconnect. |
| Provider page | `profile-web-page-url` | `http` or `https`, maximum 2048 characters, host required, embedded credentials rejected. |
| Support | `support-url` | `http`, `https`, or `tg`; same bounds and credential rejection. |
| Service logo | Any header matching `<vendor>-servicelogo`, where `<vendor>` is non-empty. Examples: `flclashx-servicelogo`, `rabbit-servicelogo`, `ajsdjddj-servicelogo`. | Header matching is case-insensitive and the vendor prefix is not allowlisted. HTTPS URL only; host required; no embedded credentials. The first valid matching value wins. Logo failure does not fail the subscription. |
| Announcement text | `announce`, then `sub-info-text`, then `banner-text` | Plain UTF-8 or `base64:`, sanitized and capped at 512 characters. Value `0` suppresses the announcement. Markup is not interpreted. |
| Announcement target | `announce-url`, then `sub-info-button-link`, then `banner-button-url` | `http`, `https`, or `tg`; bounded, with no embedded credentials. |
| Announcement label | `sub-info-button-text`, then `banner-button-text` | Sanitized plain text, maximum 32 characters. |
| Announcement tone | `sub-info-color` | Only `blue`, `green`, or `red`, mapped to semantic info/success/danger styling. |
| Simplified routes (dumb mode) | `dumb-mode` or any name ending in `-dumb-mode` (for example `flclashx-dumb-mode`) | `on`/`off` (also `true`/`false`, `yes`/`no`, `1`/`0`), case-insensitive; default `off`. When `on`, the home screen and the tray show no group tabs and only the first `proxy-groups` entry of the Mihomo config (its nodes, selection and latency). Every other group keeps working in the core; it is only not shown. Saved with the subscription and re-read on each refresh. |
| Interface language | `app-language` or any name ending in `-app-language` (for example `flclashx-app-language`) | `ru` or `en`, case-insensitive; a regional tag counts as its base language (`en-US`, `ru_RU`). Other values are ignored. The active subscription's value sets the app language until the user picks one in Settings: the user's choice always wins and the header never overrides it. Without the header the app follows the system language (Russian for `ru`, `be`, `kk`, `ky`, `tg`, `uz`; English for any other, `uk` included; Russian if it cannot be detected). Saved with the subscription and re-read on each refresh. |

Unknown metadata headers are ignored. Generalized vendor prefixes apply only to
`servicename`, `servicelogo`, `dumb-mode`, `app-language`, and the already supported
`subscription-userinfo` suffix. A bare `servicelogo` header is not accepted: the name must contain a
non-empty vendor prefix followed by `-servicelogo`. Remote styling/behavior
headers such as `flclashx-background`, `ajsdjddj-background`, widgets, settings,
or routing overrides are intentionally not supported.

### Metadata as leading `#` lines of the body

A provider that hosts a static file (a gist, object storage, a plain web server) or runs a panel that cannot set response headers may write the same metadata as comment lines at the very top of the body, the convention other clients (Happ, Hiddify, Clash family) accept:

```yaml
#profile-title: base64:8J+agCBGam9yZA==
#profile-update-interval: 12
#subscription-userinfo: upload=0; download=123; total=1073741824; expire=1798761600
#support-url: https://t.me/support
#profile-web-page-url: https://example.com
#announce: base64:0J/RgNC40LLQtdGC
#dumb-mode: on
#app-language: en
mixed-port: 7890
proxies:
  ...
```

- Every header in the table above is accepted, with the same names (case-insensitive, vendor prefixes included), formats, sanitizers and limits; values go through exactly the same code as response headers. Two are only taken from a real response: `Content-Disposition` and `x-hwid-max-devices-reached`. Unknown names are ignored.
- Line format: `#name: value`, optionally with spaces after `#` and around `:`; `name` is letters, digits and `-`. Other `#` lines (free text such as `# generated 12:00`) and blank lines are skipped. A UTF-8 BOM and CRLF line ends are fine.
- Only the leading block counts: scanning stops at the first line that is neither blank nor a `#` line, and after 64 lines or 16 KiB. A `#` line further down is an ordinary YAML comment and is not read.
- All three bodies may carry the block: the Mihomo YAML (`multicore-mihomo`), the Xray JSON (`multicore-xray`), the combined bundle (`multicore-json-massive`, before its `[`), and the Mihomo YAML string inside the bundle. The block is removed before the body is parsed, so it never reaches the core configurations. JSON cannot contain comments, so a JSON body or bundle that does not start with `#` is parsed byte for byte as before; a `#` anywhere past the leading block is still invalid JSON.
- Priority, per key, highest first: (1) real HTTP response headers (of any response used for the refresh), (2) the combined bundle's leading lines, (3) the Mihomo YAML's leading lines (inside the bundle, or the `multicore-mihomo` answer), (4) the Xray JSON's leading lines. A lower source only fills keys no higher source sent; a header that was sent keeps its key even when its value is invalid. A bundle answer is final, so sources (2)-(3) apply to a bundle, and (3)-(4) to the separate Mihomo and Xray answers. Between different keys the usual rules apply, for example a `#profile-title` line beats a file name from a `Content-Disposition` header.
- Body lines are as untrusted as headers and get no extra trust: URLs, titles and announcements pass the same sanitizers, and neither values nor the body are logged.

## Metadata limits and safety

- Maximum accepted raw metadata total: 16 KiB, shared by response headers and leading `#` body lines (headers first).
- Maximum accepted raw value per metadata field: 4 KiB.
- Header values containing CR or LF are discarded.
- URLs and subscription credentials never enter normal status responses, diagnostics, or logs.
- Control and bidirectional-control characters are removed from provider text.
- Malformed optional metadata never rejects an otherwise valid configuration.

## Backend consistency requirements

All three representations must come from one immutable backend revision.

Recommended headers on every successful config response:

```http
X-Multicore-Revision: <same immutable revision across all representations>
ETag: "<representation-specific etag>"
Cache-Control: private, no-cache
```

The current client validates body/config consistency and atomic local publication but does not yet use `X-Multicore-Revision` or `ETag` for conditional requests. Backends should still emit them so future clients can detect cross-representation revision mismatches.

## Minimal complete example

```http
HTTP/1.1 200 OK
Content-Type: application/vnd.multicore.bundle+json; charset=utf-8
Profile-Title: base64:TXVsdGlDb3JlIFByZW1pdW0=
Subscription-Userinfo: upload=1024; download=2048; total=107374182400; expire=1798761600
Profile-Update-Interval: 6
MyProvider-ServiceLogo: https://cdn.example.com/logo.png
Announce: Плановое обслуживание 20 сентября
Sub-Info-Color: blue
Dumb-Mode: off
App-Language: ru
X-Multicore-Revision: revision-42
Cache-Control: private, no-cache

["mixed-port: 7890\nproxy-groups: []\n",{"inbounds":[],"outbounds":[],"routing":{"rules":[]}}]
```

## Deep links

Both forms show a consent dialog naming only the subscription host before importing:

- `multicore://install-sub?url=<percent-encoded http(s) URL>`: the strict form; the value must be percent-encoded.
- `multicore://add/<http(s) URL>`: the Happ-style form panel subscription pages generate (for example `multicore://add/{{SUBSCRIPTION_LINK}}` in the Remnawave subpage builder). The subscription URL follows verbatim; a percent-encoded URL is also accepted.

The target must be `http`/`https` with a host, at most 8192 bytes, without spaces, control bytes or a fragment.

## Multi-subscription note

Multi-subscription support does not change this HTTP contract. Every saved profile has its own URL and independently runs the same fetch sequence. MultiCore never asks one endpoint to return several subscriptions and never merges configurations from different profile URLs.
