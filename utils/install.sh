#!/usr/bin/env bash
# Install the built server as a runnable server root, the way the C tree's `make install` does:
# the four binaries go to <prefix>/bin, the configuration templates and data to <prefix>/conf and
# <prefix>/data, the directories the daemon and the generators expect are created empty, and the
# two service files are rendered from conf/*.in for the chosen prefix and account.
#
# What git does not hold belongs to the machine and is never copied: conf/bbsd.conf,
# conf/bbsnet.conf, conf/badwords.conf, conf/ca_cert.pem and the ssh_host_* keys stay where they
# are, and --uninstall leaves them, var/ and log/ in place.
set -euo pipefail
shopt -s nullglob

prog=${0##*/}

usage() {
	cat <<EOF
usage: $prog [options]

Install the server into PREFIX as a server root (bin/, conf/, data/, log/, var/) and render
conf/lbbs.service and conf/lbbs.logrotate from the templates in conf/.

options:
  --prefix DIR         server root to install into (default /usr/local/lbbs)
  --destdir DIR        stage every path under DIR (packaging; the unit still names PREFIX)
  --bin-dir DIR        where the four binaries are (default <tree>/target/release)
  --user USER          account the unit runs as, and the owner logrotate creates (default bbs)
  --group GROUP        group of the same (default bbs)
  --systemd-dir DIR    where lbbs.service is installed (default /etc/systemd/system on Linux)
  --logrotate-dir DIR  where the logrotate drop-in is installed (default /etc/logrotate.d on Linux)
  --no-systemd         do not install the unit
  --no-logrotate       do not install the logrotate drop-in
  --uninstall          remove the installed files, keeping live files, var/ and log/
  -h, --help           print this text
EOF
}

die() {
	echo "$prog: $*" >&2
	exit 1
}

prefix=/usr/local/lbbs
destdir=
bin_dir=
user=bbs
group=bbs
unset systemd_dir logrotate_dir
action=install

while [ $# -gt 0 ]; do
	case $1 in
	--prefix) prefix=${2:?--prefix needs a directory}; shift 2 ;;
	--destdir) destdir=${2:?--destdir needs a directory}; shift 2 ;;
	--bin-dir) bin_dir=${2:?--bin-dir needs a directory}; shift 2 ;;
	--user) user=${2:?--user needs a name}; shift 2 ;;
	--group) group=${2:?--group needs a name}; shift 2 ;;
	--systemd-dir) systemd_dir=${2:?--systemd-dir needs a directory}; shift 2 ;;
	--logrotate-dir) logrotate_dir=${2:?--logrotate-dir needs a directory}; shift 2 ;;
	--no-systemd) systemd_dir=; shift ;;
	--no-logrotate) logrotate_dir=; shift ;;
	--uninstall) action=uninstall; shift ;;
	-h | --help) usage; exit 0 ;;
	*) die "unknown option: $1 (try --help)" ;;
	esac
done

# macOS and the BSDs have neither service manager, so the tree installs alone unless a directory
# was named; an explicit --systemd-dir / --logrotate-dir always wins.
if [ -z "${systemd_dir+x}" ]; then
	systemd_dir=
	[ "$(uname -s)" = Linux ] && systemd_dir=/etc/systemd/system
fi
if [ -z "${logrotate_dir+x}" ]; then
	logrotate_dir=
	[ "$(uname -s)" = Linux ] && logrotate_dir=/etc/logrotate.d
fi

case $prefix in
/*) ;;
*) die "--prefix must be an absolute path" ;;
esac
prefix=${prefix%/}
case $prefix in
*[!/]*) ;;
*) die "--prefix must not be the root directory" ;;
esac

if [ -n "$destdir" ]; then
	case $destdir in
	/*) ;;
	*) die "--destdir must be an absolute path" ;;
	esac
	destdir=${destdir%/}
	case $destdir in
	*[!/]*) ;;
	*) die "--destdir must not be the root directory" ;;
	esac
fi

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source_root=$(dirname -- "$here")
[ -d "$source_root/conf" ] && [ -d "$source_root/data" ] ||
	die "no conf/ and data/ beside $here; run this from the source tree"
bin_dir=${bin_dir:-$source_root/target/release}

case $prefix$user$group in
*"|"*) die "a value contains |, which the templates use as their delimiter" ;;
esac

pkg_root=$destdir$prefix
bins="bbsd gen_top gen_section_menu gen_ex_list"
conf_files=("$source_root"/conf/*.default "$source_root"/conf/menu.conf)
data_files=("$source_root"/data/*.txt)

if [ "$action" = uninstall ]; then
	for bin in $bins; do
		rm -f "$pkg_root/bin/$bin"
	done
	for file in "${conf_files[@]}"; do
		rm -f "$pkg_root/conf/${file##*/}"
	done
	rm -f "$pkg_root/conf/lbbs.service" "$pkg_root/conf/lbbs.logrotate"
	for file in "${data_files[@]}"; do
		rm -f "$pkg_root/data/${file##*/}"
	done
	if [ -n "$systemd_dir" ]; then
		rm -f "$destdir$systemd_dir/lbbs.service"
	fi
	if [ -n "$logrotate_dir" ]; then
		rm -f "$destdir$logrotate_dir/lbbs"
	fi
	echo "Removed the installed files from $prefix"
	echo "Left in place: the live conf files, log/ and var/. The unit, if any, is neither stopped nor disabled."
	exit 0
fi

for bin in $bins; do
	[ -x "$bin_dir/$bin" ] || die "$bin_dir/$bin is missing; run cargo build --release first"
done

install -d -m 0750 "$pkg_root" "$pkg_root/bin" "$pkg_root/conf" "$pkg_root/data" "$pkg_root/log" \
	"$pkg_root/var" "$pkg_root/var/articles" "$pkg_root/var/chicken" "$pkg_root/var/gen_ex"
for bin in $bins; do
	install -m 0755 "$bin_dir/$bin" "$pkg_root/bin/"
done
for file in "${conf_files[@]}"; do
	install -m 0644 -p "$file" "$pkg_root/conf/"
done
for file in "${data_files[@]}"; do
	install -m 0644 -p "$file" "$pkg_root/data/"
done

render() {
	sed -e "s|@prefix@|$prefix|g" -e "s|@user@|$user|g" -e "s|@group@|$group|g" "$1"
}
render "$source_root/conf/lbbs.service.in" >"$pkg_root/conf/lbbs.service"
render "$source_root/conf/lbbs.logrotate.in" >"$pkg_root/conf/lbbs.logrotate"
chmod 0644 "$pkg_root/conf/lbbs.service" "$pkg_root/conf/lbbs.logrotate"

if [ -n "$systemd_dir" ]; then
	install -d -m 0755 "$destdir$systemd_dir"
	install -m 0644 "$pkg_root/conf/lbbs.service" "$destdir$systemd_dir/lbbs.service"
fi
if [ -n "$logrotate_dir" ]; then
	install -d -m 0755 "$destdir$logrotate_dir"
	install -m 0644 "$pkg_root/conf/lbbs.logrotate" "$destdir$logrotate_dir/lbbs"
fi

echo "Installed $prefix${destdir:+ (staged under $destdir)}:"
echo "  bin/    $bins"
echo "  conf/   *.default, menu.conf, and the rendered lbbs.service / lbbs.logrotate"
echo "  data/   ${#data_files[@]} texts"
echo "  log/, var/{articles,chicken,gen_ex}"
[ -z "$systemd_dir" ] || echo "  unit    $destdir$systemd_dir/lbbs.service"
[ -z "$logrotate_dir" ] || echo "  rotate  $destdir$logrotate_dir/lbbs"
cat <<EOF

Before the first start:
  cp $prefix/conf/bbsd.conf.default $prefix/conf/bbsd.conf    # fill in db_host, db_username, db_password, db_database
  for k in rsa ed25519 ecdsa; do ssh-keygen -t \$k -f $prefix/conf/ssh_host_\${k}_key -N '' -q; done
  chown -R $user:$group $prefix

With the unit installed:
  systemctl daemon-reload && systemctl enable --now lbbs
EOF