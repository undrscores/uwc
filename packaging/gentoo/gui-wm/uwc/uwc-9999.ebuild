# Copyright 2026 undrscores
# Distributed under the terms of the MIT License

EAPI=8

# A live ebuild: it builds the git master, and cargo fetches the crates while it unpacks, so
# the build needs network access (FEATURES="-network-sandbox", e.g. through package.env).
inherit cargo git-r3

DESCRIPTION="Wayland compositor with an infinite 2D canvas and dwm-style tags (smithay)"
HOMEPAGE="https://github.com/undrscores/uwc"
EGIT_REPO_URI="https://github.com/undrscores/uwc.git"

LICENSE="MIT"
# Licenses of the crates the binary links in; fill in with `cargo license` when releasing.
LICENSE+=" Apache-2.0 BSD ISC MIT MPL-2.0 Unicode-DFS-2016"
SLOT="0"
IUSE="+portals xwayland"

DEPEND="
	dev-libs/wayland
	dev-libs/libinput:=
	media-libs/libglvnd
	media-libs/mesa[egl(+),gbm(+),gles2(+)]
	sys-auth/seatd:=
	virtual/libudev:=
	x11-libs/libdrm
	x11-libs/libxkbcommon
"
# EGL and GLES are loaded at run time, so libglvnd is a run-time dependency as well.
RDEPEND="
	${DEPEND}
	portals? (
		sys-apps/xdg-desktop-portal
		gui-libs/xdg-desktop-portal-wlr
		sys-apps/xdg-desktop-portal-gtk
	)
	xwayland? ( x11-base/xwayland )
"
BDEPEND="
	>=virtual/rust-1.88
	virtual/pkgconfig
"

QA_FLAGS_IGNORED="usr/bin/uwc"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

src_prepare() {
	default
	sed -i "s|@PREFIX@|${EPREFIX}/usr|g" uwc-session uwc.desktop || die
}

src_install() {
	cargo_src_install

	dobin uwc-session

	insinto /usr/share/wayland-sessions
	doins uwc.desktop

	if use portals; then
		insinto /usr/share/xdg-desktop-portal
		doins uwc-portals.conf
	fi

	# The built-in config doubles as the documentation: copy it to ~/.config/uwc/config.kdl.
	insinto /usr/share/uwc
	doins config.def.kdl
	doins -r shaders themes

	dodoc README.md INSTALL.md
}

pkg_postinst() {
	elog "Pick \"uwc\" in your display manager, or run uwc-session on a TTY."
	elog "uwc needs seat access: elogind or systemd-logind, or seatd with your user in the"
	elog "seat group. (sys-auth/seatd needs USE=elogind or USE=systemd for logind.)"
	elog "Config: copy ${EPREFIX}/usr/share/uwc/config.def.kdl to ~/.config/uwc/config.kdl."
	elog "Start your bar and daemons from an executable ~/.config/uwc/startup."
	use xwayland && elog "X11 apps need 'xwayland true' in the options block of your config."
}
