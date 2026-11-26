# `make install` stages the built server as a server root and renders the two service files, the
# way the C tree's `make install` does. The steps live in utils/install.sh so they can be read and
# run without make; the variables below are that script's options.
#
#   sudo make install
#   sudo make install PREFIX=/opt/lbbs
#   make install PREFIX=$HOME/lbbs SYSTEMD_UNIT_DIR= LOGROTATE_DIR=
#   sudo make uninstall PREFIX=/opt/lbbs

PREFIX ?= /usr/local/lbbs
DESTDIR ?=
BBS_USER ?= bbs
BBS_GROUP ?= bbs

# An empty variable skips that piece; outside Linux there is no service manager to install into.
ifeq ($(shell uname -s),Linux)
SYSTEMD_UNIT_DIR ?= /etc/systemd/system
LOGROTATE_DIR ?= /etc/logrotate.d
else
SYSTEMD_UNIT_DIR ?=
LOGROTATE_DIR ?=
endif

.PHONY: all build install uninstall

all: build

build:
	cargo build --release --locked

install: build
	utils/install.sh --prefix '$(PREFIX)' --user '$(BBS_USER)' --group '$(BBS_GROUP)' \
		$(if $(strip $(DESTDIR)),--destdir '$(DESTDIR)') \
		$(if $(strip $(SYSTEMD_UNIT_DIR)),--systemd-dir '$(SYSTEMD_UNIT_DIR)',--no-systemd) \
		$(if $(strip $(LOGROTATE_DIR)),--logrotate-dir '$(LOGROTATE_DIR)',--no-logrotate)

uninstall:
	utils/install.sh --uninstall --prefix '$(PREFIX)' \
		$(if $(strip $(DESTDIR)),--destdir '$(DESTDIR)') \
		$(if $(strip $(SYSTEMD_UNIT_DIR)),--systemd-dir '$(SYSTEMD_UNIT_DIR)',--no-systemd) \
		$(if $(strip $(LOGROTATE_DIR)),--logrotate-dir '$(LOGROTATE_DIR)',--no-logrotate)