# Convenience wrapper around cargo for installing a session. `make install` as root (or with
# DESTDIR/PREFIX) puts uwc, uwc-session and the display-manager entry in place.
PREFIX ?= /usr/local
DATADIR ?= $(PREFIX)/share
CARGO ?= cargo

all:
	$(CARGO) build --release

check:
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test

install:
	test -x target/release/uwc || { echo "run 'make' first (as your user, not root)"; exit 1; }
	mkdir -p $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(DATADIR)/wayland-sessions
	install -m 755 target/release/uwc $(DESTDIR)$(PREFIX)/bin/uwc
	sed 's|@PREFIX@|$(PREFIX)|g' uwc-session > $(DESTDIR)$(PREFIX)/bin/uwc-session
	chmod 755 $(DESTDIR)$(PREFIX)/bin/uwc-session
	sed 's|@PREFIX@|$(PREFIX)|g' uwc.desktop > $(DESTDIR)$(DATADIR)/wayland-sessions/uwc.desktop
	chmod 644 $(DESTDIR)$(DATADIR)/wayland-sessions/uwc.desktop

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/uwc $(DESTDIR)$(PREFIX)/bin/uwc-session \
		$(DESTDIR)$(DATADIR)/wayland-sessions/uwc.desktop

.PHONY: all check install uninstall
