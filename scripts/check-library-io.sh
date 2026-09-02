#!/usr/bin/env bash
# Design rule 3, checked on the artifact rather than the source.
#
#   ./scripts/check-library-io.sh
#
# Builds the crate WITHOUT the `cli` feature and asserts the resulting rlib
# holds no reference to std::io::stdin, std::io::stdout, or the print! machinery.
# That is what a `default-features = false` consumer links, and under stdio MCP
# those two descriptors are the JSON-RPC channel.
#
# Why not scan the source: every stdin read in the library half sits inside a
# `#[cfg(feature = "cli")]` arm, so the text is still there and a grep would
# report a violation that does not exist in the build. The symbol table reflects
# what the compiler actually kept.
#
# Why not a #[test]: a test cannot ask cargo to build a different feature set
# than the one it is running under. This has to happen outside the test harness.
#
# stderr is deliberately NOT matched — rule 3 governs stdout and stdin. The
# library writes to stderr (see finish_sync's domain note) and that is allowed.
set -euo pipefail

cd "$(dirname "$0")/.."

# Its own target dir, so this can never read an rlib left behind by a build with
# a different feature set — the one way this check could silently pass.
TARGET="target/rule3-check"
# `_+print` not `_print`: v0 mangling escapes a leading underscore, so
# `std::io::_print` (what println! lowers to) appears as `stdio6__print`. An
# earlier single-underscore pattern here matched the stdin symbols but silently
# missed every stray println! — the exact false-comfort this script exists to
# avoid. Both mutations are checked in the repo's review notes.
PATTERN='io[0-9]+std(in|out)|io[0-9]+_+print'

echo "Building the library without the \`cli\` feature..."
CARGO_TARGET_DIR="$TARGET" cargo build --no-default-features --lib --locked --quiet

rlib=$(find "$TARGET/debug" -maxdepth 1 -name 'libvault*.rlib' | head -1)
if [ -z "$rlib" ]; then
  echo "FAIL: no rlib produced at $TARGET/debug" >&2
  exit 1
fi

# `nm` must exist and must work. The previous form was
# `nm "$rlib" 2>/dev/null | grep -E ... || true`, which discarded nm's error,
# produced no output, and reported zero hits — so a machine without binutils got
# a green check for a rule nothing had verified. That is the same false comfort
# the note at the top of this file warns about, in the one line that had to be
# right.
if ! command -v nm >/dev/null 2>&1; then
  echo "FAIL: nm not found on PATH — cannot verify design rule 3." >&2
  echo "Install binutils (or llvm-nm) rather than skipping: a check that cannot" >&2
  echo "run must not report success." >&2
  exit 1
fi

if ! symbols=$(nm "$rlib" 2>&1); then
  echo "FAIL: nm could not read $rlib." >&2
  echo "$symbols" | sed 's/^/  /' >&2
  exit 1
fi

total=$(printf '%s\n' "$symbols" | grep -c . || true)
# Zero symbols is not a pass. A stripped or truncated archive greps clean for
# the same reason an empty file does, and this check's entire evidence is that
# the symbol table was read and searched.
if [ "$total" -eq 0 ]; then
  echo "FAIL: nm reported no symbols at all in $rlib." >&2
  echo "An empty symbol table cannot demonstrate the absence of stdin/stdout" >&2
  echo "references — it demonstrates that nothing was searched." >&2
  exit 1
fi

hits=$(printf '%s\n' "$symbols" | grep -E "$PATTERN" || true)
count=$(printf '%s' "$hits" | grep -c . || true)

if [ "$count" -eq 0 ]; then
  echo "OK: the library-only build references no stdin/stdout symbol."
  echo "    (checked $total symbols in $(basename "$rlib"))"
  exit 0
fi

echo "FAIL: the library-only build references $count stdin/stdout symbol(s)." >&2
echo "Design rule 3 says the library never reads stdin or writes stdout; under a" >&2
echo "stdio MCP server those descriptors carry JSON-RPC framing." >&2
echo "$hits" | sed 's/^/  /' >&2
exit 1
