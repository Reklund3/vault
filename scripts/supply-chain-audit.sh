#!/usr/bin/env bash
# Supply-chain audit for this repo: has a named bad crate EVER been in our
# dependency graph, and does what is on disk still match what the lockfile says?
#
# Written during the 2026-08-20 arrayref/proc-macro1 incident, but deliberately
# parameterised — the next advisory names different crates, not a different
# method. Edit BAD_CRATES / BAD_STRINGS and re-run.
#
#   ./scripts/supply-chain-audit.sh
#
# Exit 0 = clean, 1 = something needs a human. Every check prints what it
# examined, not just its verdict: "clean" is only meaningful if you can see the
# denominator.
set -uo pipefail

# Self-identification. Any git object containing this exact string is a revision
# of this script and is excluded from the indicator scans below. Content-based,
# not path-based, on purpose: a reset commit or a dropped stash leaves the old
# blob in the object store but unreachable, so `git rev-list -- <path>` cannot
# find it while `--batch-all-objects` still scans it. That gap made an earlier
# path-based version of this exclusion silently stop working.
SELF="scripts/supply-chain-audit.sh"
SELF_MARKER="audit-self-id:9f2c4a7e-vault-supply-chain"

# --- what we are hunting -----------------------------------------------------
# Crate names. Checked against every historical revision of Cargo.lock, not just
# the current one — a compromised dep that was added and reverted still ran its
# build script on whoever built that commit.
BAD_CRATES=(
  arrayref internment append-only-vec          # 2026-08-20: compromised maintainer
  proc-macro1 proc-macro-en                    # typosquats of proc-macro2
  aovine arone aronenao tinymember             # same campaign
)

# Free-text indicators. Pickaxed across all history and grepped over every git
# object, reachable or not.
BAD_STRINGS=(
  '23.254.165.112' 'hwsrv-798836' 'rust-setup'
  'rchaitm@gmail.com' 'dtolney'
)

# Files the payload drops.
BAD_PATHS=( /tmp/rust-setup /tmp/rust-setup.ps1 /tmp/rust-setup-launch.vbs )

# Where a crate name can appear *as a dependency*. Everywhere else — a comment,
# a doc, a commit message quoting the advisory — it is prose.
MANIFESTS="Cargo.lock Cargo.toml */Cargo.toml"

# The shapes a dependency actually takes, for scanning raw blobs where there is
# no path to filter on:
#   Cargo.lock          name = "arrayref"
#   Cargo.toml          arrayref = "0.3"   /   arrayref.workspace = true
#   Cargo.toml, table   [dependencies.arrayref]
# Anchored at line start, so prose mentioning the name never matches.
crate_alt=$(IFS='|'; echo "${BAD_CRATES[*]}")
MANIFEST_PAT="^name = \"($crate_alt)\"\$|^($crate_alt)(\.[a-z-]+)? *=|^\[[^]]*dependencies[^]]*\.($crate_alt)\]\$"

cd "$(dirname "$0")/.." || exit 1
fail=0
note() { printf '  %s\n' "$*"; }
bad()  { printf '  !! %s\n' "$*"; fail=1; }

echo "=== repo ==="
note "first commit: $(git log --reverse --format='%ad' --date=short | head -1)"
note "commits:      $(git rev-list --all --count)"

echo
echo "=== 1. every historical revision of Cargo.lock ==="
blobs=$(for rev in $(git rev-list --all -- Cargo.lock); do
          git rev-parse "$rev:Cargo.lock" 2>/dev/null
        done | sort -u)
note "distinct Cargo.lock blobs: $(echo "$blobs" | grep -c .)"
pattern="^name = \"($(IFS='|'; echo "${BAD_CRATES[*]}"))\"$"
for blob in $blobs; do
  hits=$(git cat-file -p "$blob" 2>/dev/null | grep -nE "$pattern")
  [ -n "$hits" ] && bad "blob $blob contains:" && echo "$hits" | sed 's/^/       /'
done
[ "$fail" -eq 0 ] && note "no revision ever contained a listed crate"

echo
echo "=== 2. pickaxe: did an indicator ever enter or leave history? ==="
# Exclude this script from its own search. Once it is committed, its BAD_*
# lists are repo content, and without this every run flags its own definitions
# — a detector that cries wolf on itself gets ignored, which is the real danger.
#
# The two indicator classes need different scopes, and conflating them was the
# same wolf-crying bug one level up. A crate name is only evidence when it names
# a *dependency*, which can only happen in a manifest; in prose it is someone
# writing about the incident, which this repo's own docs do in four places. So
# crate names are pickaxed over manifests only, while the payload strings (an
# IP, a dropper filename) stay scoped to everything — those have no legitimate
# reason to appear in any file.
for s in "${BAD_CRATES[@]}"; do
  n=$(git log --all --oneline -S"$s" -- $MANIFESTS 2>/dev/null | wc -l)
  [ "$n" -gt 0 ] && bad "'$s' touched by $n commit(s) in a manifest" && \
    git log --all --oneline -S"$s" -- $MANIFESTS | sed 's/^/       /'
done
for s in "${BAD_STRINGS[@]}"; do
  n=$(git log --all --oneline -S"$s" -- . ":(exclude)$SELF" 2>/dev/null | wc -l)
  [ "$n" -gt 0 ] && bad "'$s' touched by $n commit(s)" && \
    git log --all --oneline -S"$s" -- . ":(exclude)$SELF" | sed 's/^/       /'
done
note "searched ${#BAD_CRATES[@]} crate names over manifests, ${#BAD_STRINGS[@]} strings over all paths"

echo
echo "=== 3. every git object, including unreachable ==="
all_blobs=$(git cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype)' \
            | awk '$2=="blob"{print $1}')
# O(blobs) cat-file calls. Fine at this repo's size; revisit if it gets slow.
# Match the marker OR the BAD_CRATES declaration. The marker alone is not
# enough: revisions of this script committed before the marker existed are still
# in the object store, and an orphaned one (reset commit, dropped stash) cannot
# be reached by path either. `BAD_CRATES=(` appears in every revision there has
# ever been, which is what makes the exclusion retroactive.
self_blobs=$(for b in $all_blobs; do
               git cat-file -p "$b" 2>/dev/null \
                 | grep -qE "$SELF_MARKER|^BAD_CRATES=\(" && echo "$b"
             done)
objs=$(echo "$all_blobs" | grep -vxF "${self_blobs:-__none__}" || true)
note "blobs scanned: $(echo "$objs" | grep -c .) (excluded $(echo "$self_blobs" | grep -c .) revisions of this script)"
# Two greps, for the same reason section 2 has two loops. The payload strings
# are matched anywhere in a blob; the crate names only in the shapes a manifest
# uses. The previous single grep folded a bare `proc-macro1` into the loose set
# and so flagged every doc that named the incident.
blob_text=$(echo "$objs" | git cat-file --batch 2>/dev/null)
found=$(echo "$blob_text" | grep -aiE "$(IFS='|'; echo "${BAD_STRINGS[*]}")" | head)
[ -n "$found" ] && bad "indicator inside a git object:" && echo "$found" | sed 's/^/       /'
found=$(echo "$blob_text" | grep -aiE "$MANIFEST_PAT" | head)
[ -n "$found" ] && bad "dependency on a listed crate inside a git object:" && \
  echo "$found" | sed 's/^/       /'

echo
echo "=== 4. cached .crate files vs Cargo.lock checksums ==="
ok=0; mismatch=0; uncached=0
while read -r name ver sum; do
  f=$(find ~/.cargo/registry/cache -name "$name-$ver.crate" 2>/dev/null | head -1)
  if [ -z "$f" ]; then uncached=$((uncached+1)); continue; fi
  if [ "$(sha256sum "$f" | cut -d' ' -f1)" = "$sum" ]; then ok=$((ok+1))
  else mismatch=$((mismatch+1)); bad "checksum mismatch: $name-$ver"; fi
done < <(python3 -c "
import re
for blk in open('Cargo.lock').read().split('[[package]]'):
    n=re.search(r'name = \"(.*?)\"',blk); v=re.search(r'version = \"(.*?)\"',blk); c=re.search(r'checksum = \"(.*?)\"',blk)
    if n and v and c: print(n.group(1), v.group(1), c.group(1))
")
note "verified $ok, mismatched $mismatch, not cached locally $uncached"

echo
echo "=== 5. extracted build scripts (what actually runs at build time) ==="
scripts=$(find ~/.cargo/registry/src -maxdepth 3 -name build.rs 2>/dev/null)
note "build.rs files scanned: $(echo "$scripts" | grep -c .)"
sus=$(echo "$scripts" | xargs grep -lE \
  "$(IFS='|'; echo "${BAD_STRINGS[*]}")|ServerCertVerified::assertion|TcpStream|Command::new\(\"(sh|bash|powershell|cmd)" 2>/dev/null)
[ -n "$sus" ] && bad "build script matches a payload signature:" && echo "$sus" | sed 's/^/       /'

echo
echo "=== 6. dropped-file and registry-redirection indicators ==="
for p in "${BAD_PATHS[@]}"; do [ -e "$p" ] && bad "exists: $p"; done
for c in ~/.cargo/config.toml ~/.cargo/config .cargo/config.toml; do
  [ -f "$c" ] && grep -q "source" "$c" 2>/dev/null && bad "source replacement configured in $c"
done
note "checked ${#BAD_PATHS[@]} drop paths and 3 cargo config locations"

echo
if [ "$fail" -eq 0 ]; then echo "RESULT: clean"; else echo "RESULT: NEEDS REVIEW (see !! above)"; fi
exit "$fail"
