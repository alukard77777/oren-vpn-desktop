# The source archive is the existing Ubuntu 22.04 Linux stage. The pinned Go
# cores and all three Rust executables must remain byte-identical to that stage.
# Disable only binary-changing hooks and debug/build-id output, retaining RPM's
# native ELF dependency generation and the other build-root policy checks.
%global debug_package %{nil}
%global __brp_strip %{nil}
%global __brp_strip_static_archive %{nil}
%global __brp_strip_comment_note %{nil}
%global __brp_strip_lto %{nil}
%global __brp_llvm_compile_lto_elf %{nil}
%global _build_id_links none

Name:           multicore
Version:        @VERSION@
Release:        1
Summary:        Native network client combining Mihomo routing with Xray transports
License:        (MIT OR Apache-2.0) AND MPL-2.0 AND GPL-3.0-only
URL:            https://github.com/fatyzzz/multicore
Source0:        %{name}-%{version}-stage.tar.gz
BuildArch:      x86_64

# These include libraries loaded at runtime, which ELF Requires cannot discover.
Requires:       gtk3%{?_isa}
Requires:       libayatana-appindicator-gtk3%{?_isa}
Requires:       polkit
Requires:       iproute
Requires:       libxkbcommon%{?_isa}
# winit loads the X11 extension separately through xkbcommon-dl.
Requires:       libxkbcommon-x11%{?_isa}
Requires:       fontconfig%{?_isa}
Requires:       libglvnd-glx%{?_isa}
Requires:       libglvnd-egl%{?_isa}
Requires:       hicolor-icon-theme
# The always-ready service unit (/usr/lib/systemd/system/multicore.service).
Requires:       systemd

%description
MultiCore starts Mihomo and Xray behind one desktop interface. Mihomo owns
TUN, DNS, rules, and selectors; Xray provides the outbound transports.

%prep
%setup -q -c

%build
# The shared stage is already built and verified by the Linux build job.

%install
mkdir -p "%{buildroot}"
cp -a usr "%{buildroot}/"
find "%{buildroot}/usr" -type d -exec chmod 0755 {} +
find "%{buildroot}/usr" -type f -exec chmod 0644 {} +
chmod 0755 "%{buildroot}/usr/lib/multicore/multicore" \
    "%{buildroot}/usr/lib/multicore/runtime/multicore-daemon" \
    "%{buildroot}/usr/lib/multicore/runtime/multicore-core-host" \
    "%{buildroot}/usr/lib/multicore/cores/xray" \
    "%{buildroot}/usr/lib/multicore/cores/mihomo"

# Shared filesystem, desktop, icon and polkit directories belong to dependencies.
# /usr/lib/multicore is deliberate: the runtime and polkit policy use that path,
# including on x86_64 where the system shared libraries live in /usr/lib64.
%files
%defattr(-,root,root,-)
%dir /usr/lib/multicore
%dir /usr/lib/multicore/runtime
%dir /usr/lib/multicore/cores
/usr/lib/multicore/multicore
/usr/lib/multicore/runtime/multicore-daemon
/usr/lib/multicore/runtime/multicore-core-host
/usr/lib/multicore/cores/xray
/usr/lib/multicore/cores/mihomo
/usr/bin/multicore
/usr/share/applications/multicore.desktop
/usr/share/icons/hicolor/scalable/apps/multicore.svg
/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy
/usr/lib/systemd/system/multicore.service
# The vendored dashboard (packaging/linux/versions.json "dashboard" component) is a bare
# directory path (no %dir, no per-file listing): rpmbuild packages it and everything beneath
# it recursively. Its filenames are content-hashed by the upstream build and change on every
# re-vendor, so unlike the entries above they cannot be listed individually here.
/usr/lib/multicore/dashboard
%dir /usr/share/licenses/multicore
%license /usr/share/licenses/multicore/LICENSE-MIT
%license /usr/share/licenses/multicore/LICENSE-APACHE
%license /usr/share/licenses/multicore/THIRD_PARTY_NOTICES.md
%license /usr/share/licenses/multicore/Xray-core-MPL-2.0.txt
%license /usr/share/licenses/multicore/mihomo-GPL-3.0.txt
%license /usr/share/licenses/multicore/zashboard-MIT.txt

%post
# Enable and (re)start the always-ready service on install and upgrade.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
    systemctl enable multicore.service >/dev/null 2>&1 || :
    systemctl restart multicore.service >/dev/null 2>&1 || :
fi

%preun
# Final removal only ($1 == 0): stop and disable the service before its files go away.
if [ $1 -eq 0 ] && [ -d /run/systemd/system ]; then
    systemctl disable --now multicore.service >/dev/null 2>&1 || :
fi

%postun
# rpm has no purge: final removal also deletes the service's own state directory.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi
if [ $1 -eq 0 ]; then
    rm -rf /var/lib/multicore
fi
