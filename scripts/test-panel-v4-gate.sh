#!/usr/bin/env bash
#
# Fixture for scripts/check-panel-v4.sh's own exemption: the token file,
# V4.swift, is allowed to hold hand-written sizes; nothing else under
# PanelV4/ is. The exemption used to be a bare suffix match on `V4.swift:`,
# which matched every file whose NAME ends in `V4.swift` -- so a sibling like
# `SiblingV4.swift` went unchecked too. This builds a throwaway PanelV4/ with
# a sibling file that is not the token file and asserts the gate still
# fires on a hand-written size in it.
#
# Run after touching the gate: scripts/test-panel-v4-gate.sh
set -u

GATE="$(cd "$(dirname "$0")" && pwd)/check-panel-v4.sh"
T=$(mktemp -d /tmp/panel-v4-gate.XXXXXX)
trap 'rm -rf "$T"' EXIT

views="$T/apps/macos/Sources/TcrBar/PanelV4"
mkdir -p "$views"

# V4.swift: the token file. Needs >=10 "compact ? a : b" density lines for
# the gate's own positive control on that shape, and a literal INSIDE it to
# prove the token file itself stays exempt.
{
    echo "enum V4 {"
    for i in $(seq 1 10); do
        echo "    static var t$i: CGFloat { compact ? $i : $((i + 1)) }"
    done
    echo "    static let ownLiteral: CGFloat = 40"
    echo "    var body: some View { Color.clear.frame(width: 40, height: 40) }"
    echo "}"
} >"$views/V4.swift"

# A second, always-present file: the gate's own positive control on `count`
# (>=2 Swift files under PanelV4/) needs one, and it must stay clean so a
# failure never gets attributed to it by accident.
cat >"$views/PlainCard.swift" <<'EOF'
struct PlainCard: View {
    var body: some View {
        Color.clear.padding(V4.ownLiteral)
    }
}
EOF

pass=0
fail=0
check() {
    local label="$1" expect_rc="$2" out rc
    out="$("$GATE" "$T" 2>&1)"
    rc=$?
    if [ "$rc" -eq "$expect_rc" ]; then
        printf '  PASS  %-52s (exit=%s)\n' "$label" "$rc"
        pass=$((pass + 1))
    else
        printf '  FAIL  %-52s (exit=%s, wanted=%s)\n' "$label" "$rc" "$expect_rc"
        printf '%s\n' "$out" | sed 's/^/        /'
        fail=$((fail + 1))
    fi
}

echo "check-panel-v4 exemption -- fixture test"
echo

# 1. NEGATIVE CONTROL -- only the token file exists, holding its own
#    literals. Nothing to flag.
check "token file alone, its own literal -> ALLOW" 0

# 2. POSITIVE CONTROL -- a sibling whose name also ends in "V4.swift" (the
#    exact shape of the bug: PeersTabV4.swift, AccountsTabV4.swift,
#    PanelV4.swift all end in "V4.swift" without BEING the token file) gets
#    a hand-written size. The gate must fire on it.
cat >"$views/SiblingV4.swift" <<'EOF'
struct SiblingV4: View {
    var body: some View {
        Color.clear.padding(9)
    }
}
EOF
check "sibling *V4.swift with a hand-written size -> BLOCK" 1
grep -q 'SiblingV4\.swift:' <<<"$("$GATE" "$T" 2>&1)" &&
    printf '  PASS  %-52s\n' "gate names the sibling file, not just a count" &&
    pass=$((pass + 1)) ||
    { printf '  FAIL  %-52s\n' "gate names the sibling file"; fail=$((fail + 1)); }
rm "$views/SiblingV4.swift"

# 3. NEGATIVE CONTROL -- the always-present file that does NOT end in
#    V4.swift stays clean throughout; re-check with it as the only sibling.
check "unrelated clean file present -> ALLOW" 0

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
