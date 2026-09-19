#!/opt/homebrew/bin/bash
# Watch every identity and pairing security gate below go RED.
#
# One probe per gate: break the production line the gate exists to guard, run
# ONLY that gate, require a non-zero exit, restore, and at the end require the
# whole set green again. A gate that cannot be shown failing is not a gate.
#
# Every probe patches a file this test suite owns directly. The one exception
# is the peers-file mode probe: its writer (`src/config.rs::write_atomic`)
# lives in a file this suite does not own, so that probe patches the TEST to write the file with
# `std::fs::write` instead of the one writer, which proves the assertion
# measures the mode on disk rather than the writer's name.
#
# Each probe's old and new text arrive on stdin, separated by a line reading
# `@@@`, so nothing has to survive two levels of shell quoting. A patch whose
# anchor is missing is reported INVALID and fails the run: a mutation that did
# not apply is the classic way a mutation test reports success while testing
# nothing.
#
# Backups are byte copies restored with `cp`, never `git checkout`: this tree
# holds the whole run's uncommitted work.
set -uo pipefail

# The repository this script belongs to, derived from the script's own location:
# a hardcoded absolute path would be one reader's home directory in a PUBLIC repo,
# and wrong in every other checkout.
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
STAMP="$$-$(date +%s)"
LOG_DIR=/tmp/lan-p2p
BACKUP_DIR="$LOG_DIR/mutate-identity-$STAMP"
mkdir -p "$BACKUP_DIR" "$LOG_DIR"

FILES=(
  "src/peer/state.rs"
  "src/peer/pair.rs"
  "src/peer/id.rs"
  "src/peer/config.rs"
  "src/peer/discovery.rs"
  "tests/peer_noise.rs"
)
for f in "${FILES[@]}"; do
  cp "$ROOT/$f" "$BACKUP_DIR/$(echo "$f" | tr / _)"
done

restore() {
  for f in "${FILES[@]}"; do
    cp "$BACKUP_DIR/$(echo "$f" | tr / _)" "$ROOT/$f"
  done
}
trap restore EXIT INT TERM

FAILED=0

# probe <name> <cargo target flag> <test filter> <file to patch>
# stdin: OLD text, a line `@@@`, NEW text.
probe() {
  local name="$1" target="$2" filter="$3" file="$4"
  restore
  local spec="$BACKUP_DIR/$name.spec"
  cat >"$spec"
  if ! MUT_PATH="$ROOT/$file" MUT_SPEC="$spec" python3 "$ROOT/tests/tools/mutate-apply.py"; then
    echo "probe=$name INVALID: the patch did not apply (its anchor moved)"
    FAILED=1
    return
  fi
  local built exit_code
  CARGO_TARGET_DIR="$ROOT/target-mutate" cargo check --manifest-path "$ROOT/Cargo.toml" \
    --all-targets >"$LOG_DIR/mutid-$STAMP-$name-build.log" 2>&1
  built=$?
  if [ "$built" != "0" ]; then
    echo "probe=$name INVALID: the mutant does not compile ($LOG_DIR/mutid-$STAMP-$name-build.log)"
    FAILED=1
    return
  fi
  CARGO_TARGET_DIR="$ROOT/target-mutate" cargo test --manifest-path "$ROOT/Cargo.toml" \
    "$target" "$filter" >"$LOG_DIR/mutid-$STAMP-$name-test.log" 2>&1
  exit_code=$?
  echo "probe=$name gate=$filter exit=$exit_code"
  if [ "$exit_code" = "0" ]; then
    echo "FAIL: $filter stayed green with $name broken"
    FAILED=1
  fi
}

probe pairing_window_not_persisted --test=peer_noise \
  the_pairing_window_survives_a_save_and_a_load src/peer/state.rs <<'SPEC'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_window_until_ms: Option<i64>,
@@@
    #[serde(skip)]
    pub pairing_window_until_ms: Option<i64>,
SPEC

probe clock_jump_ignored --test=peer_noise \
  a_clock_that_moved_closes_the_pairing_window src/peer/state.rs <<'SPEC'
if now_ms >= opened_at_ms && now_ms < until_ms {
@@@
if now_ms < until_ms {
SPEC

probe enrolment_writes_nothing --test=peer_noise \
  a_two_sided_enrolment_leaves_a_pinned_row_on_both_sides src/peer/pair.rs <<'SPEC'
    file.peers.push(row.clone());
    save(peers_path, &file)?;
@@@
    file.peers.push(row.clone());
SPEC

probe invite_not_spent --test=peer_noise \
  a_one_use_invite_refuses_its_second_joiner src/peer/pair.rs <<'SPEC'
        invite.uses_left = invite.uses_left.saturating_sub(1);
        invite.clone()
    };
    if invite.uses_left == 0 {
        file.pending_invites.remove(position);
    }
@@@
        invite.clone()
    };
SPEC

probe enrolment_label_unsanitized --test=peer_noise \
  a_hostile_enrolment_label_is_refused_and_spends_nothing src/peer/pair.rs <<'SPEC'
    let label = sanitize_label(&enroll.label)
        .map_err(|refusal| anyhow!("peer enrol: the joiner's label is refused: {refusal}"))?;
@@@
    let label = enroll.label.clone();
SPEC

probe peers_file_mode --test=peer_noise \
  the_peers_file_is_written_at_0600_and_a_wider_mode_is_refused tests/peer_noise.rs <<'SPEC'
    teamclaude_rs::peer::config::save(path, &file).expect("the peers file is written");
@@@
    std::fs::write(path, serde_json::to_string(&file).expect("json")).expect("written");
SPEC

probe peers_mode_check --test=peer_noise \
  the_peers_file_is_written_at_0600_and_a_wider_mode_is_refused src/peer/config.rs <<'SPEC'
            if mode != 0o600 {
@@@
            if false {
SPEC

probe key_file_follows_symlink --test=peer_noise \
  a_planted_symlink_does_not_capture_the_node_key src/peer/id.rs <<'SPEC'
        .create_new(true)
        .mode(mode)
@@@
        .create(true)
        .truncate(true)
        .mode(mode)
SPEC

probe stdin_ignores_its_reader --test=peer_noise \
  a_join_token_arrives_on_stdin_and_never_in_a_message src/peer/pair.rs <<'SPEC'
    reader
        .read_line(&mut line)
        .context("peer join: could not read the join key from standard input")?;
@@@
    let _ignored = &mut reader;
SPEC

probe refusal_echoes_the_secret --test=peer_noise \
  a_join_token_arrives_on_stdin_and_never_in_a_message src/peer/pair.rs <<'SPEC'
            .context("peer join: the secret field is not 32 base32-encoded bytes")?;
@@@
            .with_context(|| format!("peer join: the secret field {secret:?} is not 32 base32-encoded bytes"))?;
SPEC

probe beacon_name_denylist --lib \
  peer::discovery::tests::a_hostile_beacon_name_is_refused src/peer/discovery.rs <<'SPEC'
    let name = tcr_peer_wire::sanitize_label(raw).ok()?;
@@@
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 32 || trimmed.contains('@') {
        return None;
    }
    let name = trimmed.to_string();
SPEC

probe inbound_name_unsanitized --lib \
  peer::discovery::tests::an_inbound_hostile_name_is_dropped_and_the_row_survives src/peer/discovery.rs <<'SPEC'
        name: name.and_then(sanitize_name),
@@@
        name: name.map(str::to_string),
SPEC

restore
CARGO_TARGET_DIR="$ROOT/target-mutate" cargo test --manifest-path "$ROOT/Cargo.toml" \
  --test peer_noise --test peer_wire --test peer_discovery \
  >"$LOG_DIR/mutid-$STAMP-restored.log" 2>&1
green=$?
CARGO_TARGET_DIR="$ROOT/target-mutate" cargo test --manifest-path "$ROOT/Cargo.toml" \
  --lib peer:: >"$LOG_DIR/mutid-$STAMP-restored-lib.log" 2>&1
green_lib=$?
echo "restored_exit=$green restored_lib_exit=$green_lib"
[ "$green" = "0" ] && [ "$green_lib" = "0" ] || FAILED=1

if [ "$FAILED" = "0" ]; then
  echo "ALL PROBES CAUGHT"
else
  echo "MUTATION TEST FAILED"
fi
exit "$FAILED"
