#!/usr/bin/env bash
# Watch the new Tools-tab tests FAIL. Mutates five production lines, runs the
# two new suites, restores every byte and re-runs green.
#
# Scratch paths carry $$ because several of these worktrees run at once.
set -uo pipefail

root="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
core="$root/apps/macos/Sources/TcrBarCore/FleetStatus.swift"
machine="$root/apps/macos/Sources/TcrBarCore/MachineStats.swift"
backup="/tmp/tools-tab-mutate-$$"
mkdir -p "$backup"
cp "$core" "$backup/FleetStatus.swift"
cp "$machine" "$backup/MachineStats.swift"
restore() {
  cp "$backup/FleetStatus.swift" "$core"
  cp "$backup/MachineStats.swift" "$machine"
}
trap restore EXIT

# 1. the wire key goes back to the snake_case nobody sends
sed -i 's/case tool, calls, errors, secondsP50, overOneMinute/case tool, calls, errors\n        case secondsP50 = "seconds_p50"\n        case overOneMinute = "over_one_minute"/' "$core"
# shellcheck disable=SC2016  # the Swift source we mutate contains $0/$1 literally
# 2. running calls stop sorting oldest-first
sed -i 's/^        .sorted { ($0.call.startedMs ?? .max) < ($1.call.startedMs ?? .max) }$//' "$core"
# 3. an unreported timeouts-by-class becomes an empty card instead of no card
sed -i 's/guard !counts.isEmpty else { return nil }/guard true else { return nil }/' "$core"
# 4. the median becomes a calls-weighted mean, which one Agent bucket can drag
sed -i 's/if Double(seen) >= Double(total) \/ 2 { return bucket.p50 }/if Double(seen) >= Double(total) { return bucket.p50 }/' "$core"
# 5. the amber band loses its top half, so 2x cores reads red
sed -i 's/if perCore <= 2 { return .busy }/if perCore < 2 { return .busy }/' "$machine"

echo "=== mutated; expecting FAILURES ==="
(cd "$root/apps/macos" && swift test --filter "MachineStatsTests|ToolsTabDataTests" 2>&1 |
  grep -E "error:|XCTAssert.*failed|' failed \(|Executed [0-9]+ tests")

restore
trap - EXIT
echo "=== restored; expecting GREEN ==="
cmp "$backup/FleetStatus.swift" "$core" && cmp "$backup/MachineStats.swift" "$machine" &&
  echo "restore: byte-identical"
(cd "$root/apps/macos" && swift test --filter "MachineStatsTests|ToolsTabDataTests" 2>&1 |
  grep -E "Executed [0-9]+ tests")
rm -rf "$backup"
