# RPM spec for OpenBlock — draft, built from milestone MS-16 onward.
#
# This is a COPR package: it bundles the Rust crate graph in the source archive
# (see packaging/README.md). An official Fedora repository package requires
# unbundling via cargo2rpm and is tracked as OPEN-021.
#
# Build locally from a release tarball:
#   rpmbuild -ba packaging/fedora/openblock.spec
#
# The archive must contain Cargo.lock and the full workspace. `%cargo_*` macros
# are deliberately not used yet: the dependency graph is resolved from the
# bundled vendor directory produced by `cargo vendor` in the release tarball.
%global debug_package %{nil}

Name:           openblock
Version:        0.0.1
Release:        1%{?dist}
Summary:        Fast, deeply moddable, server-first voxel sandbox

License:        MIT OR Apache-2.0
URL:            https://github.com/martintondel02/openblock
Source0:        %{url}/releases/download/v%{version}/openblock-%{version}.tar.gz
# Bundled Rust crates; see packaging/README.md and OPEN-021.
Provides:       bundled(crate(openblock)) = %{version}

BuildRequires:  gcc gcc-c++ make
BuildRequires:  rust cargo
BuildRequires:  pkgconf-pkg-config
BuildRequires:  alsa-lib-devel wayland-devel libxkbcommon-devel
BuildRequires:  vulkan-loader-devel fontconfig-devel freetype-devel
BuildRequires:  systemd-devel systemd-rpm-macros
Requires:       mesa-vulkan-drivers

%description
OpenBlock is an open-source voxel sandbox game. Mine, build, explore and
survive alone or with friends on servers you host yourself. It targets the
Minecraft Java feature set as a baseline and goes further with a scriptable
content platform, GPU-driven rendering and server-first multiplayer.

This package installs the client with desktop integration. The dedicated
server is in the openblock-server subpackage.

%package server
Summary:        Dedicated server for OpenBlock
Requires(pre):  shadow-utils
Requires:       systemd
Requires(post): systemd
Requires(preun): systemd
Requires(postun): systemd
%description server
The headless OpenBlock dedicated server. No GPU, display server or audio
device is required. Includes a hardened systemd unit, a firewalld service
definition for the QUIC/UDP game port, and a dedicated system user.

%prep
%autosetup -n openblock-%{version}

%build
export CARGO_HOME="${PWD}/.cargo-home"
export CARGO_NET_OFFLINE=true
%set_build_flags
cargo build --release --locked --offline --workspace \
  --bin openblock --bin openblock-server

%install
install -Dpm 0755 target/release/openblock %{buildroot}%{_bindir}/openblock
install -Dpm 0755 target/release/openblock-server %{buildroot}%{_bindir}/openblock-server

install -Dpm 0644 packaging/fedora/io.github.martintondel02.OpenBlock.desktop \
  %{buildroot}%{_datadir}/applications/io.github.martintondel02.OpenBlock.desktop
install -Dpm 0644 packaging/fedora/openblock.metainfo.xml \
  %{buildroot}%{_metainfodir}/io.github.martintondel02.OpenBlock.metainfo.xml
install -Dpm 0644 packaging/fedora/io.github.martintondel02.OpenBlock.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.martintondel02.OpenBlock.svg
install -Dpm 0644 packaging/fedora/openblock.mime.xml \
  %{buildroot}%{_datadir}/mime/packages/openblock.xml

install -Dpm 0644 packaging/fedora/openblock-server.service \
  %{buildroot}%{_unitdir}/openblock-server.service
install -Dpm 0644 packaging/fedora/openblock-server.firewalld.xml \
  %{buildroot}%{_prefix}/lib/firewalld/services/openblock-server.xml
install -Dpm 0644 packaging/fedora/openblock-server.sysusers \
  %{buildroot}%{_sysusersdir}/openblock-server.conf
install -dpm 0750 %{buildroot}%{_localstatedir}/lib/openblock-server
install -Dpm 0640 packaging/fedora/server.toml \
  %{buildroot}%{_sysconfdir}/openblock-server/server.toml

%pre server
getent group openblock-server >/dev/null || groupadd -r openblock-server
getent passwd openblock-server >/dev/null || \
  useradd -r -g openblock-server -d %{_localstatedir}/lib/openblock-server \
  -s /sbin/nologin -c "OpenBlock dedicated server" openblock-server
exit 0

%post
update-desktop-database &>/dev/null || :
if command -v appstreamcli >/dev/null 2>&1; then appstreamcli refresh --source=os &>/dev/null || :; fi

%post server
%systemd_post openblock-server.service
%firewalld_reload

%preun server
%systemd_preun openblock-server.service

%postun server
%systemd_postun_with_restart openblock-server.service

%postun
update-desktop-database &>/dev/null || :

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md CHANGELOG.md
%{_bindir}/openblock
%{_datadir}/applications/io.github.martintondel02.OpenBlock.desktop
%{_metainfodir}/io.github.martintondel02.OpenBlock.metainfo.xml
%{_datadir}/icons/hicolor/scalable/apps/io.github.martintondel02.OpenBlock.svg
%{_datadir}/mime/packages/openblock.xml

%files server
%{_bindir}/openblock-server
%{_unitdir}/openblock-server.service
%{_prefix}/lib/firewalld/services/openblock-server.xml
%{_sysusersdir}/openblock-server.conf
%attr(0750,openblock-server,openblock-server) %dir %{_localstatedir}/lib/openblock-server
%config(noreplace) %{_sysconfdir}/openblock-server/server.toml

%changelog
* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.0-1
- Initial packaging skeleton (specification stage; not yet a playable build).
