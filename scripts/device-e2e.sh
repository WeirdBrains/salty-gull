#!/bin/sh
# End-to-end update test on a real machine (Linux, FreeBSD, OpenBSD), with real binaries:
# a local salty-server, a portable install run through salty-launch, a good update, a broken
# update that rolls back, and on the BSDs a package installed through doas.
#
# Usage: scripts/device-e2e.sh BIN_DIR
# BIN_DIR holds saltyctl, salty-server, salty-launch, salty-demo-1.0.0, salty-demo-1.1.0,
# salty-demo-1.2.0-broken and salty-demo-1.3.0 (built with SALTY_DEMO_VERSION / SALTY_DEMO_BROKEN).
# Everything lives in a temp dir. The doas step needs a doas rule for `pkg`/`pkg_add` (on a
# desktop doas asks for the password; a test VM uses a nopass rule) and is skipped without one.
set -eu
BIN=$(cd "${1:?usage: device-e2e.sh BIN_DIR}" && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/salty-e2e.XXXXXX")
PORT=${SALTY_E2E_PORT:-18470}
SERVER=http://127.0.0.1:$PORT
OS=$(uname -s)
ARCH=x86-64
ok() { echo "ok - $*"; }
die() { echo "not ok - $*" >&2; exit 1; }
cleanup() { [ -n "${SPID:-}" ] && kill "$SPID" 2>/dev/null; rm -rf "$T"; }
trap cleanup EXIT INT TERM

case $OS in
  Linux) PLATFORM=linux ;;
  FreeBSD) PLATFORM=freebsd ;;
  OpenBSD) PLATFORM=openbsd ;;
  *) die "unsupported OS $OS" ;;
esac

# --- publisher: keys, root, server
cd "$T"
"$BIN/saltyctl" key-new root.key >/dev/null
REL=$("$BIN/saltyctl" key-new release.key)
PIN=$("$BIN/saltyctl" root-new --org demo --sign root.key --release-key "$REL" --out root.cbor)
mkdir srv
SALTY_TOKEN=$("$BIN/salty-server" --home srv admin-token)
export SALTY_TOKEN
"$BIN/salty-server" --home srv serve --org demo --bind "127.0.0.1:$PORT" >server.log 2>&1 &
SPID=$!
i=0; until "$BIN/saltyctl" publish-root root.cbor --server "$SERVER" >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -gt 50 ] && { cat server.log; die "server did not start"; }; sleep 0.2
done
ok "server up, root published ($PLATFORM)"

SEQ=0
# publish CHANNEL VERSION FORMAT DIR
publish() {
  SEQ=$((SEQ + 1))
  "$BIN/saltyctl" manifest --app demo --channel "$1" --seq "$SEQ" --version "$2" --platform "$PLATFORM" \
    --arch "$ARCH" --format "$3" --signing-identity demo-test --key release.key --dir "$4" --out "m$SEQ.cbor" >/dev/null
  "$BIN/saltyctl" publish "m$SEQ.cbor" --dir "$4" --server "$SERVER" >/dev/null
}
demo_config() { # CHANNEL FORMAT STATE
  printf '{"server":"%s","pin":"%s","app":"demo","channel":"%s","state_dir":"%s","platform":"%s","arch":"%s","format":"%s","signing_identity":"demo-test"}\n' \
    "$SERVER" "$PIN" "$1" "$3" "$PLATFORM" "$ARCH" "$2"
}
# tarball NAME DEMO_BINARY -> pkg/NAME/demo.tar.gz with bin/salty-demo and its config
tarball() {
  mkdir -p "build-$1/bin" "pkg-$1"
  cp "$BIN/$2" "build-$1/bin/salty-demo"
  demo_config portable tar-gz "$T/unused" >"build-$1/bin/salty-demo.json"
  tar -czf "pkg-$1/demo.tar.gz" -C "build-$1" bin
}

# --- portable install: launcher + shipped 1.0.0
I=$T/install
mkdir -p "$I/app/bin"
cp "$BIN/salty-launch" "$I/demo"
cp "$BIN/salty-demo-1.0.0" "$I/app/bin/salty-demo"
demo_config portable tar-gz "$T/unused" >"$I/app/bin/salty-demo.json"
printf '{"app_dir":"app","entry":"bin/salty-demo","state_dir":"state"}\n' >"$I/salty-launch.json"
out=$("$I/demo" run) || die "first start: $out"
[ "$out" = "salty-demo 1.0.0 ready" ] || die "first start printed: $out"
ok "launcher runs the shipped 1.0.0"

tarball 1.1.0 salty-demo-1.1.0
publish portable 1.1.0 tar-gz "pkg-1.1.0"
out=$("$I/demo" update) || die "update to 1.1.0: $out"
echo "$out" | grep -q "staged demo-portable-1" || die "update printed: $out"
out=$("$I/demo" run) || die "trial start: $out"
[ "$out" = "salty-demo 1.1.0 ready (committed demo-portable-1)" ] || die "trial start printed: $out"
out=$("$I/demo" run); [ "$out" = "salty-demo 1.1.0 ready" ] || die "after commit printed: $out"
ok "1.1.0 downloaded, verified, swapped in, trial-booted and committed"

tarball 1.2.0 salty-demo-1.2.0-broken
publish portable 1.2.0 tar-gz "pkg-1.2.0"
"$I/demo" update >/dev/null || die "update to 1.2.0"
for n in 1 2; do
  if out=$("$I/demo" run 2>&1); then die "broken build $n started: $out"; fi
  echo "$out" | grep -q "1.2.0 is a broken build" || die "broken start $n printed: $out"
done
out=$("$I/demo" run) || die "start after rollback: $out"
[ "$out" = "salty-demo 1.1.0 ready" ] || die "after rollback printed: $out"
[ ! -e "$I/state/versions/demo-portable-2" ] || die "rolled-back build still on disk"
if out=$("$I/demo" update 2>&1); then die "rolled-back update offered again: $out"; fi
echo "$out" | grep -qi "bad" || die "refusal printed: $out"
ok "broken 1.2.0 failed twice, rolled back to 1.1.0 and is refused from then on"

# --- tampering: a blob changed on the server is refused
tarball 1.3.0 salty-demo-1.3.0
publish portable 1.3.0 tar-gz "pkg-1.3.0"
BLOBS=$(find srv -type d -name blobs | head -1)
[ -n "$BLOBS" ] || die "no blob dir in the server home"
NEW=$(ls -t "$BLOBS" | head -1)
chmod u+w "$BLOBS/$NEW"; printf 'X' | dd of="$BLOBS/$NEW" bs=1 count=1 conv=notrunc 2>/dev/null
if out=$("$I/demo" update 2>&1); then die "tampered update accepted: $out"; fi
[ ! -e "$I/state/versions/demo-portable-4" ] || die "tampered build staged"
ok "a tampered package is refused: $(echo "$out" | tail -1)"

# --- BSD packages through doas
case $PLATFORM in
  freebsd|openbsd) ;;
  *) echo "# no BSD package step on $PLATFORM"; exit 0 ;;
esac
S=$T/stage; mkdir -p "$S/usr/local/bin" pkgout
cp "$BIN/salty-demo-1.3.0" "$S/usr/local/bin/salty-demo"
demo_config bsdpkg bsd-pkg "$T/pkgstate" >"$S/usr/local/bin/salty-demo.json"
if [ "$PLATFORM" = freebsd ]; then
  cat >pkgout/+MANIFEST <<M
name: salty-demo
version: "1.3.0"
origin: sysutils/salty-demo
comment: Salty Gull test app
desc: Salty Gull end-to-end test app
maintainer: m3talsmith@gmail.com
www: https://github.com/WeirdBrains/salty-gull
prefix: /usr/local
M
  printf '/usr/local/bin/salty-demo\n/usr/local/bin/salty-demo.json\n' >pkgout/plist
  pkg create -q -M pkgout/+MANIFEST -p pkgout/plist -r "$S" -o pkgout
  DEL="pkg delete -y salty-demo"
else
  printf 'Salty Gull end-to-end test app\n' >pkgout/desc
  printf '@cwd /usr/local\nbin/salty-demo\nbin/salty-demo.json\n' >pkgout/plist
  pkg_create -D COMMENT="Salty Gull test app" -D MAINTAINER="m3talsmith@gmail.com" -D FULLPKGPATH=sysutils/salty-demo \
    -d pkgout/desc -f pkgout/plist -B "$S" -p /usr/local pkgout/salty-demo-1.3.0.tgz
  DEL="pkg_delete salty-demo"
fi
mkdir -p pkgfile; mv pkgout/salty-demo-1.3.0.* pkgfile/
publish bsdpkg 1.3.0 bsd-pkg pkgfile
P=$T/pkgapp; mkdir -p "$P"
cp "$BIN/salty-demo-1.0.0" "$P/salty-demo"
demo_config bsdpkg bsd-pkg "$T/pkgstate" >"$P/salty-demo.json"
"$P/salty-demo" run >/dev/null

out=$("$P/salty-demo" update 2>&1) || {
  echo "$out" | grep -q "doas" || die "package update failed without reaching doas: $out"
  echo "# doas refused (no rule for this user): $(echo "$out" | tail -1)"
  [ ! -e /usr/local/bin/salty-demo ] || die "installed without doas"
  ok "without a doas rule nothing is installed"
  exit 0
}
echo "$out" | grep -q "running doas" || die "package update printed: $out"
[ -x /usr/local/bin/salty-demo ] || die "package not installed"
out=$(/usr/local/bin/salty-demo run) || die "packaged start: $out"
[ "$out" = "salty-demo 1.3.0 ready (committed demo-bsdpkg-$SEQ)" ] || die "packaged start printed: $out"
doas $DEL >/dev/null
[ ! -e /usr/local/bin/salty-demo ] || die "package not removed"
ok "1.3.0 $PLATFORM package installed through doas, trial-booted, committed and removed"
