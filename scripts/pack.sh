#!/usr/bin/env bash
# Build the news Space App and assemble its installable zip.
#
#   release/news/                <- staged, flat install layout
#     news[.exe]                 (release binary; manifest runtime.start points at it)
#     senclaw-manifest.json  senclaw-hub.json
#     skills/ personas/ web_dist/
#   dist/<platform>/news-app.zip           <- the artifact you install / publish
#
# Usage: scripts/pack.sh [--skip-build]
#
# Windows note: the daemon launches a server app with `cmd /C <runtime.start>`,
# and cmd cannot run `./name` — so on Windows the binary is staged as
# `news.exe` and the manifest inside the zip has `runtime.start`
# rewritten to match. The manifest in the source tree is never touched.
set -euo pipefail

SKIP_BUILD="${1:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ID=news
BIN=news
ZIPNAME=news-app.zip
# <source> <dest-in-zip>
STAGE_DIRS=(
  "skills skills"
  "personas personas"
  "web/dist web_dist"
)

# ── platform ────────────────────────────────────────────────────────────────
# Names match `senclaw`'s own host_platform(): darwin|linux|windows + arm64|x64.
# The hub stores one artifact per (version, platform), so this string is what
# a user's daemon matches against when it downloads.
case "$(uname -s)" in
  Darwin)                     OS=darwin  ; EXE=""     ;;
  Linux)                      OS=linux   ; EXE=""     ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT) OS=windows ; EXE=".exe" ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64)  ARCH=x64   ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
PLATFORM="$OS-$ARCH"

STAGE="$ROOT/release/$ID"
OUTDIR="$ROOT/dist/$PLATFORM"
ZIP="$OUTDIR/$ZIPNAME"

echo "==> packing $ID for $PLATFORM"

# ── build ───────────────────────────────────────────────────────────────────
if [[ "$SKIP_BUILD" != "--skip-build" ]]; then
  echo "==> npm build: web"
  ( cd "$ROOT/web" && npm install --silent && npm run build )

  echo "==> cargo build --release"
  ( cd "$ROOT" && cargo build --release --locked )
fi

BINPATH="$ROOT/target/release/$BIN$EXE"
[[ -f "$BINPATH" ]] || { echo "missing $BINPATH — run without --skip-build" >&2; exit 1; }

# ── stage ───────────────────────────────────────────────────────────────────
rm -rf "$STAGE" "$ZIP"
mkdir -p "$STAGE" "$OUTDIR"

cp "$BINPATH" "$STAGE/$BIN$EXE"
chmod +x "$STAGE/$BIN$EXE"

# The manifest is the one file that differs per platform: cmd.exe cannot run
# `./name`, so Windows zips carry `runtime.start = "name.exe"`.
if [[ -n "$EXE" ]]; then
  jq --arg s "$BIN$EXE" '.runtime.start = $s' \
    "$ROOT/senclaw-manifest.json" > "$STAGE/senclaw-manifest.json"
  echo "    runtime.start -> $BIN$EXE (Windows)"
else
  cp "$ROOT/senclaw-manifest.json" "$STAGE/senclaw-manifest.json"
fi

for pair in "${STAGE_DIRS[@]}"; do
  src="${pair%% *}"; dest="${pair#* }"
  [[ -e "$ROOT/$src" ]] || { echo "    skip $src (absent)"; continue; }
  cp -R "$ROOT/$src" "$STAGE/$dest"
done

# senclaw-hub.json rides along so `senclaw hub publish` and the CI publisher
# read the same version whether they start from the zip or the source tree.
cp "$ROOT/senclaw-hub.json" "$STAGE/senclaw-hub.json"

find "$STAGE" -name '.DS_Store' -delete 2>/dev/null || true

# ── zip ─────────────────────────────────────────────────────────────────────
# `zip` is absent from GitHub's Windows runners; 7-Zip is present on all three.
if command -v zip >/dev/null 2>&1; then
  ( cd "$STAGE" && zip -rqX "$ZIP" . )
elif command -v 7z >/dev/null 2>&1; then
  ( cd "$STAGE" && 7z a -tzip -bso0 -bsp0 "$(cygpath -w "$ZIP" 2>/dev/null || echo "$ZIP")" . >/dev/null )
else
  echo "need either 'zip' or '7z' on PATH to build the artifact" >&2
  exit 1
fi

# `senclaw hub publish --pack` looks for the artifact beside the app by
# default, so leave a copy at the repo root for hand-publishing from a dev
# machine. CI publishes out of dist/<platform>/ and never reads this one.
cp "$ZIP" "$ROOT/$ZIPNAME"

SIZE=$(wc -c < "$ZIP" | tr -d ' ')
echo "done: $ZIP ($(( SIZE / 1024 )) KB)"

# The hub holds the whole upload in memory to hash it, so 20 MB is a hard
# server limit — better to fail here than after a five-minute upload.
if (( SIZE > 20 * 1024 * 1024 )); then
  echo "WARNING: $ZIPNAME is $(( SIZE / 1048576 )) MB — the hub rejects anything over 20 MB" >&2
fi
