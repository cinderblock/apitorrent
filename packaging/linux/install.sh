#!/bin/sh
# Install apitorrentd as a systemd service on Linux, or upgrade or remove it.
#
#   curl -fsSL https://github.com/cinderblock/apitorrent/releases/latest/download/install.sh | sudo sh
#   curl -fsSL .../install.sh | sudo sh -s -- --user deluge
#   sudo sh install.sh --uninstall
#
# The daemon goes in /opt/apitorrent, owned by the service user, so it can update
# itself (Settings → Updates in the web UI, or `apitorrentd update`): it replaces its
# own executable and exits for systemd to restart it. Running this script again upgrades
# to the latest release and keeps the choices made the first time.
#
# Installed from a .deb or .rpm instead? Then apt or dnf updates it; don't use this.
set -eu

REPO=cinderblock/apitorrent
PREFIX=/opt/apitorrent
UNIT=/etc/systemd/system/apitorrentd.service
CONF_DIR=/etc/apitorrent
ENV_FILE=$CONF_DIR/apitorrentd.env
STATE=$CONF_DIR/install.conf
LINK=/usr/local/bin/apitorrentd

usage() {
	cat <<'EOF'
Usage: install.sh [options]

  --user USER          Run as this existing user. Moving from Deluge? Use Deluge's user,
                       so apitorrent can read its config and share its files.
                       Default: a new system user, "apitorrent".
  --data-dir DIR       Database and state. Default: /var/lib/apitorrent
  --download-dir DIR   Where new downloads go. Default: <data dir>/downloads
  --version vX.Y.Z     Install this release instead of the latest.
  --binary FILE        Install this apitorrentd executable instead of downloading one
                       (a build of your own). It won't be replaced until an update.
  --no-start           Install, but don't start or enable the service.
  --uninstall          Stop the service and remove the program. Keeps the data
                       directory and /etc/apitorrent.
  -h, --help           This help.

Options you don't give keep the values of the last install.
EOF
}

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# The last install's choices: defaults for this run, which flags override.
SVC_USER='' DATA_DIR='' DOWNLOAD_DIR='' FRESH=1
if [ -r "$STATE" ]; then
	FRESH=
	# shellcheck disable=SC1090
	. "$STATE"
fi
PREV_USER=$SVC_USER PREV_DATA_DIR=$DATA_DIR

VERSION='' BINARY='' START=1 UNINSTALL=''
while [ $# -gt 0 ]; do
	case "$1" in
		--user) [ $# -ge 2 ] || die "$1 needs a value"; SVC_USER=$2; shift 2 ;;
		--data-dir) [ $# -ge 2 ] || die "$1 needs a value"; DATA_DIR=$2; shift 2 ;;
		--download-dir) [ $# -ge 2 ] || die "$1 needs a value"; DOWNLOAD_DIR=$2; shift 2 ;;
		--version) [ $# -ge 2 ] || die "$1 needs a value"; VERSION=$2; shift 2 ;;
		--binary) [ $# -ge 2 ] || die "$1 needs a value"; BINARY=$2; shift 2 ;;
		--no-start) START=; shift ;;
		--uninstall) UNINSTALL=1; shift ;;
		-h | --help) usage; exit 0 ;;
		*) usage >&2; die "unknown option: $1" ;;
	esac
done

[ "$(id -u)" -eq 0 ] || die "run this as root (sudo sh install.sh ...)"
[ "$(uname -s)" = Linux ] || die "this installer is for Linux"
[ -d /run/systemd/system ] || die "systemd isn't running here; run apitorrentd another way (see the README)"

if [ -n "$UNINSTALL" ]; then
	if [ -f "$UNIT" ]; then
		systemctl disable --now apitorrentd.service 2>/dev/null || true
		rm -f "$UNIT"
		systemctl daemon-reload
	fi
	[ -L "$LINK" ] && rm -f "$LINK"
	rm -rf "$PREFIX"
	rm -f "$STATE"
	say "apitorrentd is removed."
	[ -n "$DATA_DIR" ] && say "Kept: the data directory, $DATA_DIR, and $CONF_DIR."
	exit 0
fi

if [ -f /usr/lib/systemd/system/apitorrentd.service ] || [ -f /lib/systemd/system/apitorrentd.service ]; then
	die "apitorrentd is installed from a package; apt or dnf updates it"
fi

: "${DATA_DIR:=/var/lib/apitorrent}"
: "${DOWNLOAD_DIR:=$DATA_DIR/downloads}"
case "$DATA_DIR" in /*) ;; *) die "--data-dir must be an absolute path" ;; esac
case "$DOWNLOAD_DIR" in /*) ;; *) die "--download-dir must be an absolute path" ;; esac

# --- The program ---------------------------------------------------------------------

case "$(uname -m)" in
	x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
	aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
	*) die "no apitorrentd build for $(uname -m) yet" ;;
esac

fetch() { # URL FILE
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q --https-only -O "$2" "$1"
	else
		die "need curl or wget"
	fi
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	else
		shasum -a 256 "$1" | cut -d' ' -f1
	fi
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

if [ -n "$BINARY" ]; then
	[ -f "$BINARY" ] || die "no such file: $BINARY"
	cp "$BINARY" "$TMP/apitorrentd"
	chmod 755 "$TMP/apitorrentd"
	say "Installing $BINARY."
else
	if [ -n "$VERSION" ]; then
		base=https://github.com/$REPO/releases/download/$VERSION
	else
		base=https://github.com/$REPO/releases/latest/download
	fi
	say "Fetching the release manifest..."
	fetch "$base/apitorrentd-manifest.json" "$TMP/manifest.json" \
		|| die "couldn't download $base/apitorrentd-manifest.json"
	# The manifest lists one asset per target, one field per line: target, url, sha256.
	# shellcheck disable=SC2046 # two words wanted: the URL and the hash
	set -- $(awk -v t="\"$TARGET\"" '
		$1 == "\"target\":" { hit = ($2 == t "," || $2 == t) }
		hit && $1 == "\"url\":" { gsub(/[",]/, "", $2); url = $2 }
		hit && $1 == "\"sha256\":" { gsub(/[",]/, "", $2); print url, $2; exit }
	' "$TMP/manifest.json")
	[ $# -eq 2 ] || die "the release has no build for $TARGET"
	url=$1 want=$2
	version=$(awk -F'"' '$2 == "version" { print $4; exit }' "$TMP/manifest.json")
	say "Downloading apitorrentd $version for $TARGET..."
	fetch "$url" "$TMP/apitorrentd" || die "couldn't download $url"
	got=$(sha256 "$TMP/apitorrentd")
	[ "$got" = "$want" ] || die "the download's SHA-256 is $got, but the manifest says $want"
	chmod 755 "$TMP/apitorrentd"
fi
# --- The user ------------------------------------------------------------------------

if [ -z "$SVC_USER" ]; then
	SVC_USER=apitorrent
	if ! id "$SVC_USER" >/dev/null 2>&1; then
		nologin=$(command -v nologin || echo /usr/sbin/nologin)
		useradd --system --user-group --home-dir "$DATA_DIR" --no-create-home \
			--shell "$nologin" --comment apitorrent "$SVC_USER"
		say "Made the system user $SVC_USER."
	fi
fi
id "$SVC_USER" >/dev/null 2>&1 || die "no user called $SVC_USER"
SVC_GROUP=$(id -gn "$SVC_USER")

# --- Files ---------------------------------------------------------------------------

was_running=
systemctl is-active --quiet apitorrentd.service 2>/dev/null && was_running=1

# The program's directory belongs to the service user: self-update replaces the
# executable in place.
install -d -m 755 -o "$SVC_USER" -g "$SVC_GROUP" "$PREFIX"
install -m 755 -o "$SVC_USER" -g "$SVC_GROUP" "$TMP/apitorrentd" "$PREFIX/apitorrentd.new"
# Tried here, not in $TMP: /tmp is often mounted noexec.
if ! "$PREFIX/apitorrentd.new" --version >/dev/null 2>&1; then
	why=$("$PREFIX/apitorrentd.new" --version 2>&1 | head -n 1)
	rm -f "$PREFIX/apitorrentd.new"
	die "the new apitorrentd doesn't run here (too old a glibc?): $why"
fi
mv -f "$PREFIX/apitorrentd.new" "$PREFIX/apitorrentd"
ln -sf "$PREFIX/apitorrentd" "$LINK"

if [ ! -d "$DATA_DIR" ]; then
	install -d -m 750 -o "$SVC_USER" -g "$SVC_GROUP" "$DATA_DIR"
elif [ -n "$PREV_USER" ] && [ "$PREV_USER" != "$SVC_USER" ] && [ "$PREV_DATA_DIR" = "$DATA_DIR" ]; then
	# Ours, made for the previous user: it moves to the new one.
	chown -R "$SVC_USER:$SVC_GROUP" "$DATA_DIR"
	say "$DATA_DIR now belongs to $SVC_USER."
fi
[ -d "$DOWNLOAD_DIR" ] || install -d -m 755 -o "$SVC_USER" -g "$SVC_GROUP" "$DOWNLOAD_DIR"
can_write() { runuser -u "$SVC_USER" -- test -w "$1"; }
can_write "$DATA_DIR" || die "$SVC_USER can't write to $DATA_DIR"
can_write "$DOWNLOAD_DIR" || say "Warning: $SVC_USER can't write to $DOWNLOAD_DIR, so downloads there will fail."

install -d -m 755 "$CONF_DIR"
if [ ! -f "$ENV_FILE" ]; then
	cat >"$ENV_FILE" <<'EOF'
# apitorrentd's settings, as environment variables. Every command-line option has one:
# see `apitorrentd --help`. Restart after a change: sudo systemctl restart apitorrentd
# This file is yours; install.sh never changes it once written.

# Where the API and web UI listen. Loopback only: reach it through an SSH tunnel
# (ssh -L 8742:localhost:8742 this-server) or a reverse proxy. See "Reaching it from
# other machines" in the README before listening elsewhere.
#APITORRENT_API=127.0.0.1:8742

# While no user exists, also write the setup token to <data dir>/setup-token.
APITORRENT_WRITE_SETUP_TOKEN=true

# Built-in HTTPS, for a desktop app on your network (it shows a code to compare):
#APITORRENT_TLS_SELF_SIGNED=true
EOF
	chmod 640 "$ENV_FILE"
	chgrp "$SVC_GROUP" "$ENV_FILE"
fi

cat >"$STATE" <<EOF
# install.sh's choices, used as the defaults next time it runs.
SVC_USER='$SVC_USER'
DATA_DIR='$DATA_DIR'
DOWNLOAD_DIR='$DOWNLOAD_DIR'
EOF

cat >"$UNIT" <<EOF
# Written by install.sh, which rewrites it on every run. Change settings in
# $ENV_FILE, or with \`systemctl edit apitorrentd\`.
[Unit]
Description=apitorrent daemon
Documentation=https://github.com/$REPO#readme
Wants=network-online.target
After=network-online.target

[Service]
User=$SVC_USER
Group=$SVC_GROUP
Environment=APITORRENT_DATA_DIR=$DATA_DIR
Environment=APITORRENT_DOWNLOAD_DIR=$DOWNLOAD_DIR
EnvironmentFile=-$ENV_FILE
ExecStart=$PREFIX/apitorrentd
# After installing an update the daemon exits with 75, to be started again as the new
# version.
Restart=on-failure
RestartSec=3
TimeoutStopSec=60
LimitNOFILE=65536

NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
# Not ProtectHome: torrents often live in home directories.

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload

if [ -z "$START" ]; then
	say "Installed. Start it with: sudo systemctl enable --now apitorrentd"
	exit 0
fi

if [ -n "$was_running" ]; then
	systemctl restart apitorrentd.service
else
	systemctl enable --now apitorrentd.service
fi

# --- What next -----------------------------------------------------------------------

sleep 2
if ! systemctl is-active --quiet apitorrentd.service; then
	journalctl -u apitorrentd.service -n 30 --no-pager >&2 || true
	die "apitorrentd didn't start; the log is above"
fi
say ""
say "apitorrentd $("$PREFIX/apitorrentd" --version | awk '{ print $2 }') is running as $SVC_USER."
say "  Data:      $DATA_DIR"
say "  Downloads: $DOWNLOAD_DIR"
say "  Settings:  $ENV_FILE"
say "  Log:       journalctl -u apitorrentd -f"

# A first install has no admin yet: the daemon writes a setup token for one.
token=
if [ -n "$FRESH" ]; then
	i=0
	while [ $i -lt 15 ] && [ ! -s "$DATA_DIR/setup-token" ]; do
		i=$((i + 1))
		sleep 1
	done
	[ -s "$DATA_DIR/setup-token" ] && token=$(cat "$DATA_DIR/setup-token")
fi

host=$(hostname -f 2>/dev/null || hostname)
say ""
say "To open it from your computer, tunnel its port over SSH:"
say ""
say "  ssh -L 8742:localhost:8742 $host"
say ""
if [ -n "$token" ]; then
	say "then create the first admin at"
	say ""
	say "  http://localhost:8742/setup#token=$token"
	say ""
	say "The token works once and expires after an hour. A new one: sudo cat $DATA_DIR/setup-token"
else
	say "then open http://localhost:8742"
fi
say ""
say "The desktop app connects through the same tunnel: add a server at http://localhost:8742."
