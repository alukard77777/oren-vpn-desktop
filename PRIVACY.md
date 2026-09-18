# Privacy

MultiCore is a native Windows client. It does not operate a MultiCore account,
analytics service, telemetry endpoint, or hosted subscription service. The
application does not intentionally send usage analytics or crash reports to
the project.

## Data stored on this Windows device

Runtime state is stored under `%LOCALAPPDATA%\\MultiCore`. Depending on the
features used, this can include:

- subscription URLs and the downloaded subscription snapshot/configuration;
- selected profiles, route selections, window placement, and other
  preferences;
- local device identity and runtime state needed by the client; and
- diagnostic output from the bundled cores.

The latest persistent core log is kept at
`%LOCALAPPDATA%\\MultiCore\\logs\\latest-core.log` and is capped at 1 MiB.
Diagnostic output is redacted by the application before it is retained or
shown, but users should still review and redact logs before sharing them.
The application does not claim to encrypt these files. File access is subject
to the Windows account and filesystem permissions.

The installer removes the installed program and its installation-specific
state, but it does not promise to remove `%LOCALAPPDATA%\\MultiCore` data.
Delete that directory separately after exiting MultiCore if you want to remove
local profiles, subscription data, preferences, and logs. The updater may
retain temporary rollback/staging directories under the installation directory
until its cleanup completes.

## Network requests

When a user imports or refreshes a subscription, MultiCore sends requests to
the provider URL selected by that user. The subscription fetcher uses these
documented User-Agent values for the corresponding requests:

- `multicore-json-massive` for the combined Mihomo/Xray response;
- `multicore-mihomo` for the Mihomo YAML response;
- `multicore-xray` for the Xray JSON response; and
- `multicore-service-logo` when fetching a provider-supplied service logo.

The provider can therefore receive the network information normally visible to
an HTTP service, such as the request IP, timing, and requested URL. Provider
privacy practices are outside MultiCore's control. Subscription contents and
provider metadata are processed locally for the client UI and runtime.

The desktop and daemon communicate through authenticated local loopback
interfaces. This local traffic is part of the application and is not a
MultiCore-operated remote service.

The updater checks GitHub Releases only when a release repository has been
configured into the build. It requests the configured repository's latest
stable release metadata and, when an update is selected, its release asset.
Without that configuration, no GitHub update channel is enabled. GitHub's own
privacy practices apply to those requests.

## Elevated Windows components

To provide the Mihomo TUN functionality, the packaged runtime can launch the
bundled cores through `multicore-core-host` with the Windows elevation required
by the release. The elevated host runs locally, communicates with the desktop
and daemon through the authenticated local broker, and does not create a
MultiCore network service. Windows security and account policies still apply.

## Changes

This notice describes the current client behavior and may change with future
releases. Check the release source and packaging documentation for the version
you install.
