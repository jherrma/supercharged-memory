#!/usr/bin/env bash
# Build the release artifacts for every supported platform into dist/.
#
# The premise of the native rewrite (issue #17) is "download one binary and
# you're done", which only holds if every artifact comes out of ONE machine
# reproducibly. This script is that machine's procedure.
#
# x86_64-apple-darwin is deliberately NOT in the matrix: Intel Macs are
# practically extinct, and carrying a target nobody runs costs a build slot and
# an asset that would never be tested on its own hardware.
#
#   ./build.sh                     # every supported target
#   ./build.sh --list              # the matrix, and whether each is buildable here
#   ./build.sh <triple> [...]      # a subset
#   ./build.sh --clean             # remove dist/ first
#
# A target whose toolchain is missing FAILS the run. It is never skipped: a
# release that silently lacks a platform is worse than a release that did not
# happen, because nobody notices until a user on that platform does.
#
# Cross-compilation is done with cargo-zigbuild rather than `cross`: zig is a
# single downloaded binary that already carries every libc and the mingw
# headers, while `cross` needs a running Docker daemon and one image per target.
# Neither can conjure Apple's SDK: the darwin rows of --list say what is missing.
set -euo pipefail

cd "$(dirname "$0")"

# triple|os|arch|extension|builder
TARGETS=(
  "x86_64-unknown-linux-musl|linux|amd64||zigbuild"
  "aarch64-unknown-linux-musl|linux|arm64||zigbuild"
  "x86_64-pc-windows-gnu|windows|amd64|.exe|zigbuild"
  "aarch64-pc-windows-gnullvm|windows|arm64|.exe|zigbuild"
  "aarch64-apple-darwin|macos|arm64||zigbuild-sdk"
)

DIST=dist
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

die() { printf 'build.sh: %s\n' "$*" >&2; exit 1; }
field() { echo "$1" | cut -d'|' -f"$2"; }

# The commit and the date are computed ONCE and passed to every target, so all
# six artifacts of a release report the same --version. Letting build.rs guess
# per target would stamp two different dates on a run that crosses midnight UTC.
if git rev-parse --short HEAD >/dev/null 2>&1; then
  SM_COMMIT=$(git rev-parse --short HEAD)
  [ -n "$(git status --porcelain)" ] && SM_COMMIT="${SM_COMMIT}-dirty"
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
else
  SM_COMMIT=unknown
  SOURCE_DATE_EPOCH=$(date -u +%s)
fi
export SM_COMMIT SOURCE_DATE_EPOCH

# `zig` and `cargo` are found via PATH, which a non-interactive shell (Claude
# Code's Bash tool, a cron job, CI) does not get from ~/.zshrc. Add the usual
# locations rather than failing with a confusing "cargo: not found".
# tools/ carries the `windres` shim the Windows targets need -- see the file for
# why a bare `windres` is missing on a distribution that ships only prefixed
# mingw binutils. It is prepended unconditionally because only a Windows build
# ever invokes it.
export PATH="$PWD/tools:$HOME/.cargo/bin:$HOME/.local/zig:$PATH"

# Is this target buildable on this machine? Echoes the reason it is not.
why_not() {
  local triple="$1" builder="$2"
  command -v cargo >/dev/null || { echo "cargo is not on PATH"; return; }
  rustup target list --installed 2>/dev/null | grep -qx "$triple" \
    || { echo "run: rustup target add $triple"; return; }
  case "$builder" in
    zigbuild|zigbuild-sdk)
      command -v zig >/dev/null || { echo "zig is not on PATH (https://ziglang.org/download/)"; return; }
      command -v cargo-zigbuild >/dev/null || { echo "run: cargo install cargo-zigbuild"; return; }
      ;;
  esac
  if [ "$builder" = "zigbuild-sdk" ] && [ -z "${SDKROOT:-}" ]; then
    # Apple ships no cross-linkable SDK for Linux and its licence does not allow
    # redistributing one, so this is a policy limit, not a missing package.
    # Point SDKROOT at a macOS SDK, or build the darwin artifacts on a Mac / in
    # CI (story 017).
    echo "SDKROOT is unset — point it at a macOS SDK, or build the darwin artifacts on a Mac / in CI (story 017)"
    return
  fi
  echo ""
}

# The header comment IS the help text, so the two cannot drift apart. Printed
# from line 2 up to the first line that is not a comment.
usage() {
  awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
  exit "${1:-0}"
}

list() {
  printf '%-28s %-8s %-6s %s\n' TARGET OS ARCH STATUS
  for t in "${TARGETS[@]}"; do
    local reason
    reason=$(why_not "$(field "$t" 1)" "$(field "$t" 5)")
    printf '%-28s %-8s %-6s %s\n' "$(field "$t" 1)" "$(field "$t" 2)" \
      "$(field "$t" 3)" "${reason:-buildable}"
  done
}

build_one() {
  local t="$1"
  local triple os arch ext builder reason out name
  triple=$(field "$t" 1); os=$(field "$t" 2); arch=$(field "$t" 3)
  ext=$(field "$t" 4); builder=$(field "$t" 5)

  reason=$(why_not "$triple" "$builder")
  [ -n "$reason" ] && die "cannot build $triple: $reason"

  echo "==> $triple ($os/$arch)"
  cargo zigbuild --release --target "$triple"

  out="target/$triple/release/supercharged-memory$ext"
  [ -f "$out" ] || die "$triple built but produced no $out"
  name="supercharged-memory_${VERSION}_${os}_${arch}${ext}"
  install -Dm755 "$out" "$DIST/$name"
  echo "    $DIST/$name  ($(du -h "$DIST/$name" | cut -f1))"
}

targets_named() {
  local want="$1" t
  for t in "${TARGETS[@]}"; do
    [ "$(field "$t" 1)" = "$want" ] && { echo "$t"; return 0; }
  done
  die "unknown target '$want' — run ./build.sh --list"
}

main() {
  local selected=()
  local clean=0
  while [ $# -gt 0 ]; do
    case "$1" in
      -h|--help) usage 0 ;;
      --list) list; exit 0 ;;
      --clean) clean=1 ;;
      -*) die "unknown option '$1' — run ./build.sh --help" ;;
      *) selected+=("$(targets_named "$1")") ;;
    esac
    shift
  done
  [ ${#selected[@]} -eq 0 ] && selected=("${TARGETS[@]}")

  echo "supercharged-memory $VERSION  commit $SM_COMMIT"
  # Every target is checked BEFORE the first one is built, so a missing
  # toolchain costs seconds rather than being discovered after five successful
  # cross-builds -- and a run that cannot finish leaves no half-filled dist/.
  local t reason
  for t in "${selected[@]}"; do
    reason=$(why_not "$(field "$t" 1)" "$(field "$t" 5)")
    [ -n "$reason" ] && die "cannot build $(field "$t" 1): $reason"
  done

  [ "$clean" = 1 ] && rm -rf "$DIST"
  mkdir -p "$DIST"
  for t in "${selected[@]}"; do build_one "$t"; done

  # Regenerated over everything currently in dist/, not just what this run
  # built, so a subset build cannot leave a checksums file that disagrees with
  # the directory beside it.
  ( cd "$DIST" && find . -maxdepth 1 -type f ! -name checksums.txt -printf '%P\n' \
      | sort | xargs sha256sum > checksums.txt )
  echo
  echo "==> $DIST/checksums.txt"
  cat "$DIST/checksums.txt"
}

main "$@"
