#!/bin/bash
# Cross-build saltyctl, salty-server and salty-client for FreeBSD and OpenBSD from Linux.
# Needs clang, ld.lld, llvm-ar, rustup (stable, plus the pinned nightly below with rust-src for OpenBSD,
# a tier-3 target): rustup toolchain install nightly-2026-11-12 --profile minimal -c rust-src
# Usage: scripts/cross-bsd.sh SYSROOT_DIR [freebsd|openbsd|all]
# Sysroots are fetched from the official mirrors and checked against the release checksums.
# Run cargo with CARGO_TARGET_DIR outside the repo.
set -euo pipefail
root=$(realpath -m "${1:?usage: cross-bsd.sh SYSROOT_DIR [freebsd|openbsd|all]}"); which=${2:-all}
FREEBSD=14.5; OPENBSD=8.0
# Nightlies before 2026-11-04 panic in std::env::current_exe on OpenBSD (fixed by rust-lang/rust#163393).
OPENBSD_NIGHTLY=${OPENBSD_NIGHTLY:-nightly-2026-11-12}
mkdir -p "$root"

wrapper() { # name clang-target sysroot
  cat > "$root/$1" <<W
#!/bin/bash
# Drops host library paths (some build scripts add /usr/lib) so only the sysroot is linked.
args=(); skip=0
for a in "\$@"; do
  if [ \$skip = 1 ]; then skip=0; [[ "\$a" == /usr/lib* || "\$a" == /lib* ]] && continue; args+=(-L "\$a"); continue; fi
  case "\$a" in -L) skip=1; continue;; -L/usr/lib*|-L/lib*) continue;; esac
  args+=("\$a")
done
exec clang --target=$2 --sysroot=$3 -fuse-ld=lld "\${args[@]}"
W
  chmod +x "$root/$1"
}

freebsd() {
  local s=$root/freebsd-$FREEBSD url=https://download.freebsd.org/releases/amd64/$FREEBSD-RELEASE
  if [ ! -d "$s/usr/include" ]; then
    mkdir -p "$s"; curl -sfL "$url/base.txz" -o "$root/base.txz"
    want=$(curl -sfL "$url/MANIFEST" | awk '$1=="base.txz"{print $2}')
    echo "$want  $root/base.txz" | sha256sum -c -
    tar -xf "$root/base.txz" -C "$s" ./usr/include ./usr/lib ./lib; rm "$root/base.txz"
  fi
  wrapper fbsd-cc x86_64-unknown-freebsd14 "$s"
  rustup target add x86_64-unknown-freebsd
  CC_x86_64_unknown_freebsd=$root/fbsd-cc AR_x86_64_unknown_freebsd=llvm-ar \
  CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER=$root/fbsd-cc \
    cargo build --locked --release --target x86_64-unknown-freebsd -p saltyctl -p salty-server -p salty-client -p salty-launch
}

openbsd() {
  local s=$root/openbsd-$OPENBSD v=${OPENBSD/./} url=https://cdn.openbsd.org/pub/OpenBSD/$OPENBSD/amd64
  if [ ! -d "$s/usr/include" ]; then
    mkdir -p "$s"; curl -sfL "$url/SHA256" -o "$root/SHA256"
    for f in base$v.tgz comp$v.tgz; do
      curl -sfL "$url/$f" -o "$root/$f"
      (cd "$root" && grep -F "($f)" SHA256 | sed -E 's/SHA256 \((.*)\) = (.*)/\2  \1/' | sha256sum -c -)
      tar -xzf "$root/$f" -C "$s" ./usr/include ./usr/lib; rm "$root/$f"
    done
    rm "$root/SHA256"
    (cd "$s/usr/lib" && for f in lib*.so.*; do b=${f%%.so.*}; [ -e "$b.so" ] || ln -s "$f" "$b.so"; done)
  fi
  wrapper obsd-cc x86_64-unknown-openbsd "$s"
  CC_x86_64_unknown_openbsd=$root/obsd-cc AR_x86_64_unknown_openbsd=llvm-ar \
  CARGO_TARGET_X86_64_UNKNOWN_OPENBSD_LINKER=$root/obsd-cc \
    cargo "+$OPENBSD_NIGHTLY" build --locked -Zbuild-std=std,panic_abort --release --target x86_64-unknown-openbsd \
      -p saltyctl -p salty-server -p salty-client -p salty-launch
}

case $which in freebsd) freebsd;; openbsd) openbsd;; all) freebsd; openbsd;; *) echo "unknown: $which" >&2; exit 2;; esac
