#!/usr/bin/env bash
# Fixture for the pre-commit "working notes" gate.
#
# Builds a throwaway repository, slices ONLY that gate out of the real hook so
# no other gate can take the blame or the credit, and runs five cases through
# it. Run it after touching the gate:  .githooks/test-working-notes-gate.sh
#
# Case 5 exists because the first version of this fixture reported a false
# failure: a rejected commit leaves its files STAGED, so the next case inherited
# them and looked like an overreach. Each rejecting case now unstages after
# itself.
set -u
HOOK="$(cd "$(dirname "$0")" && pwd)/pre-commit"
T=$(mktemp -d /tmp/working-notes-gate.XXXXXX); trap 'rm -rf "$T"' EXIT
cd "$T" || exit 1
git init -q .; git config user.email tcr-fixture@example.com; git config user.name tcr-fixture
mkdir -p .githooks docs/design src data/plans

python3 - "$HOOK" <<'PY'
import pathlib, sys
src = pathlib.Path(sys.argv[1]).read_text()
i = src.index("# --- Working notes never enter a public repository")
j = src.index("\nfi\n", src.index('if [ -n "$staged_notes" ]; then')) + len("\nfi\n")
pathlib.Path(".githooks/pre-commit").write_text("#!/usr/bin/env bash\nset -u\n" + src[i:j] + "\nexit 0\n")
PY
chmod +x .githooks/pre-commit

echo seed > README.md; echo pre-existing > docs/design/legacy.md
git add README.md docs/design/legacy.md; git commit -q --no-verify -m initial
git config core.hooksPath .githooks

fails=0
run() { git commit -q -m "$1" >/dev/null 2>&1; echo $?; }
check() { # name want_nonzero actual
  if [ "$2" = yes ] && [ "$3" -eq 0 ]; then echo "FAIL $1: accepted, must reject"; fails=$((fails+1));
  elif [ "$2" = no ] && [ "$3" -ne 0 ]; then echo "FAIL $1: rejected, must accept"; fails=$((fails+1));
  else echo "ok   $1"; fi
}

echo x > docs/design/new-note.md; git add docs/design/new-note.md
check "new design doc is refused" yes "$(run c1)"
git restore --staged docs/design/new-note.md; rm -f docs/design/new-note.md

printf 'fn main() {}\n' > src/main.rs; git add src/main.rs
check "ordinary source passes" no "$(run c2)"

echo edited >> docs/design/legacy.md; git add docs/design/legacy.md
check "editing an already-tracked design doc passes" no "$(run c3)"

echo y > data/plans/p.md; git add data/plans/p.md
check "new data/plans file is refused" yes "$(run c4)"
git restore --staged data/plans/p.md; rm -rf data/plans

echo z > docs/architecture-note.md; git add docs/architecture-note.md
check "docs/ outside design/ passes (no overreach)" no "$(run c5)"

[ "$fails" -eq 0 ] && echo "working-notes gate: all cases correct" || { echo "working-notes gate: $fails case(s) wrong"; exit 1; }
