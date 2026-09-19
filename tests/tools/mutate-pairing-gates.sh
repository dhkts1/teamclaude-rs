#!/opt/homebrew/bin/bash
# Mutation-prove these gates: break the exact failure each one exists
# to catch, run it, and require RED.
#
# A verification script that exits 0 proves it ran, not that it checked. So
# every row below is a mutation the gate MUST reject, and this script fails if
# any of them stays green.
#
# Every mutation is applied to the real file in the tree and restored from a
# byte copy taken first. `git checkout` is NOT used: this worktree holds
# uncommitted work and a restore that reached for git would wipe it.
set -uo pipefail

# Derived from the checkout this script lives in, never written down: this
# repository is public, and an absolute home path in a committed file is the
# thing the pre-commit disclosure gate exists to stop.
ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target-mutate}"
# A unique scratch dir, because five lanes may be running at once and a fixed
# /tmp path lets one run report a verdict about another run's file.
SCRATCH="$(mktemp -d "/tmp/mutate-pairing-$$-XXXXXX")"
LOG="$SCRATCH/run.log"

declare -a BACKED_UP=()

restore_all() {
  local entry file
  for entry in "${BACKED_UP[@]:-}"; do
    [[ -n "$entry" ]] || continue
    file="${entry%%::*}"
    # `cp` WITHOUT `-p`, then an explicit `touch`. Measured the hard way: with
    # `cp -p` the restored file carried the BACKUP's mtime, which is older than
    # the mutated write, so cargo judged the crate up to date and re-ran the
    # binary it had built from the MUTATED source. Three gates then failed on a
    # clean tree that `git status` called clean, and the control row failed for
    # a reason nothing on disk could explain. Restoring the bytes is half a
    # restore; the build has to be told.
    cp "$SCRATCH/${entry##*::}" "$ROOT/$file"
    touch "$ROOT/$file"
  done
  BACKED_UP=()
}
trap 'restore_all; rm -rf "$SCRATCH"' EXIT INT TERM

backup() {
  local file="$1" tag
  tag="$(printf '%s' "$file" | tr '/.' '__')"
  cp -p "$ROOT/$file" "$SCRATCH/$tag"
  BACKED_UP+=("$file::$tag")
}

PASS=0
FAIL=0

# mutate <label> <file> <test-binary> <test-name> <python-mutation>
mutate() {
  local label="$1" file="$2" binary="$3" test_name="$4" mutation="$5"
  backup "$file"
  if ! MUT_FILE="$ROOT/$file" python3 -c "$mutation"; then
    printf '%s: FAIL: mutation-could-not-apply: %s\n' "$file" "$label"
    FAIL=$((FAIL + 1))
    restore_all
    return
  fi
  local rc
  cargo test --manifest-path "$ROOT/Cargo.toml" --test "$binary" "$test_name" \
    >"$LOG" 2>&1
  rc=$?
  restore_all
  if [[ $rc -ne 0 ]]; then
    printf '%s: PASS: red-as-required: %s\n' "$file" "$label"
    PASS=$((PASS + 1))
  else
    printf '%s: FAIL: STAYED GREEN: %s (log %s)\n' "$file" "$label" "$LOG"
    printf '  the gate %s did not catch: %s\n' "$test_name" "$label"
    FAIL=$((FAIL + 1))
  fi
}

# --- Control: with nothing mutated, every gate under test is GREEN. -----------
# Without this, a script whose `cargo test` invocation was simply broken would
# report every row RED and call it a success.
if cargo test --manifest-path "$ROOT/Cargo.toml" \
     --test peer_wire --test peer_pairing --test peer_discovery \
     --test peer_noise --test peer_refusal_log >"$LOG" 2>&1; then
  printf 'control: PASS: every gate is green before any mutation\n'
  PASS=$((PASS + 1))
else
  printf 'control: FAIL: the gates are not green before any mutation (log %s)\n' "$LOG"
  printf '  every RED below would then be meaningless\n'
  FAIL=$((FAIL + 1))
  exit 1
fi

# --- Item 3: a NEW serializable wire type is caught ---------------------------
mutate 'a new pub struct with a credential field, which every OTHER gate misses' \
  'crates/tcr-peer-wire/src/lib.rs' peer_wire the_allowlist_covers_every_serializable_wire_type '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
s += """

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub access_token_blob: String,
}
"""
open(p, "w").write(s)
'

# The same mutation against the OTHER two credential gates, to show what this
# script exists to demonstrate: they stay green, which is why item 3 was asked
# for. Inverted on purpose: these two rows PASS when the gate stays green.
for pair in "wire_has_no_credential_field" "every_wire_type_serializes_only_allowlisted_keys"; do
  backup 'crates/tcr-peer-wire/src/lib.rs'
  MUT_FILE="$ROOT/crates/tcr-peer-wire/src/lib.rs" python3 -c '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
s += """

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub access_token_blob: String,
}
"""
open(p, "w").write(s)
'
  cargo test --manifest-path "$ROOT/Cargo.toml" --test peer_wire "$pair" >"$LOG" 2>&1
  rc=$?
  restore_all
  if [[ $rc -eq 0 ]]; then
    printf 'crates/tcr-peer-wire/src/lib.rs: NOTE: %s stays GREEN on a new credential-carrying type\n' "$pair"
    printf '  that is the documented gap item 3 closes, not a defect in this run\n'
    PASS=$((PASS + 1))
  else
    printf 'crates/tcr-peer-wire/src/lib.rs: NOTE: %s went RED too, which is better than recorded\n' "$pair"
    PASS=$((PASS + 1))
  fi
done

# --- Item 1: the enrolment caller ---------------------------------------------
mutate 'the listener never calls accept_enrolment, so a headless join pins nothing' \
  'src/peer/listener.rs' peer_pairing a_two_process_join_leaves_a_pinned_row_on_both_sides '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        return serve_enrolment(stream, &mut session, context, &secret).await;"
assert old in s, "the enrolment dispatch line moved"
s = s.replace(old, "        let _ = &secret;")
open(p, "w").write(s)
'

# --- Item 2: the one-use invite lock ------------------------------------------
mutate 'accept_enrolment without the file lock double-accepts a one-use invite' \
  'src/peer/pair.rs' peer_pairing two_concurrent_joiners_on_one_use_invite_leave_exactly_one_ok '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    let _lock = crate::peer::config::FileLock::acquire(peers_path)?;\n    let mut file = read_or_default(peers_path)?;\n\n    let Some(position)"
assert old in s, "the lock line moved"
s = s.replace(old, "    let mut file = read_or_default(peers_path)?;\n\n    let Some(position)")
open(p, "w").write(s)
'

# --- Item 5: the authenticated refusal bound ----------------------------------
mutate 'every authenticated failure logs, so a reconnect loop fills the disk' \
  'src/peer/listener.rs' peer_refusal_log an_authenticated_reconnect_loop_writes_one_log_line '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "                let admitted = log.admit(&addr, now_ms());"
assert old in s, "the admit call moved"
s = s.replace(old, "                let admitted = if failure.authenticated { Some(0) } else { log.admit(&addr, now_ms()) };")
open(p, "w").write(s)
'

# --- Item 6: announceName defaults off ----------------------------------------
mutate 'announceName back to default-on, so a fresh config announces a name' \
  'src/peer/config.rs' peer_discovery a_fresh_config_announces_no_name '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    #[serde(default)]\n    pub announce_name: bool,"
assert old in s, "the announce_name field moved"
s = s.replace(old, "    #[serde(default = \"default_announce_name\")]\n    pub announce_name: bool,")
s = s.replace("fn default_max_hops() -> u8 {", "fn default_announce_name() -> bool {\n    true\n}\n\nfn default_max_hops() -> u8 {")
# The DEFAULT impl is what the gate reads, so derive(Default) has to move too.
old_derive = "#[derive(Debug, Clone, Default, Serialize, Deserialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct PeerFile {"
assert old_derive in s, "the PeerFile derive moved"
s = s.replace(old_derive, "#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct PeerFile {")
s = s.replace("fn default_announce_name() -> bool {", """impl Default for PeerFile {
    fn default() -> Self {
        Self {
            listen: None,
            discovery: false,
            name: None,
            announce_name: true,
            network_key: None,
            max_hops: default_max_hops(),
            peers: Vec::new(),
            pending_invites: Vec::new(),
        }
    }
}

fn default_announce_name() -> bool {""")
open(p, "w").write(s)
'

# --- Item 9: the accepted-window gating ---------------------------------------
mutate 'XX answered with no accepted window, disclosing this node static key' \
  'src/peer/listener.rs' peer_pairing xx_from_an_unaccepted_instance_gets_zero_bytes '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if !peer_state.accepted_window(addr, &instance_id, now_ms) {"
assert old in s, "the accepted-window check moved"
s = s.replace(old, "        if false {")
open(p, "w").write(s)
'

mutate 'the accepted window keyed on the address only, so an id changer inherits it' \
  'src/peer/state.rs' peer_pairing an_accepted_window_admits_one_instance_id_only '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "                && &open.instance_id == instance_id\n"
assert old in s, "the instance-id comparison moved"
s = s.replace(old, "")
open(p, "w").write(s)
'

mutate 'a muted address still reaches the pending queue' \
  'src/peer/state.rs' peer_pairing a_knock_from_a_muted_address_gets_zero_bytes '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if self.is_muted(addr, now_ms) {\n            return Err(KnockRefusal::Muted);\n        }"
assert old in s, "the mute check moved"
s = s.replace(old, "")
open(p, "w").write(s)
'

mutate 'a banned KEY is admitted from a new address, so DHCP undoes a block' \
  'src/peer/listener.rs' peer_pairing a_banned_key_is_refused_from_a_new_address '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "                if banned.iter().any(|ban| ban.key.as_ref() == Some(&pinned)) {"
assert old in s, "the key-ban check moved"
s = s.replace(old, "                if false {")
open(p, "w").write(s)
'

# --- Item 10: the caps --------------------------------------------------------
mutate 'the knock rate limit removed, so a flood is unbounded' \
  'src/peer/listener.rs' peer_pairing the_fourth_knock_in_ten_seconds_gets_nothing '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if !guard.take_knock_token(addr, now_ms) {"
assert old in s, "the token-bucket check moved"
s = s.replace(old, "        if !guard.take_knock_token(addr, now_ms) && false {")
open(p, "w").write(s)
'

mutate 'the pending-queue cap removed, so a /24 of hosts makes 254 rows' \
  'src/peer/state.rs' peer_pairing the_ninth_outstanding_knock_is_refused '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if self.pending.len() >= MAX_PENDING_KNOCKS {"
assert old in s, "the queue cap moved"
s = s.replace(old, "        if false {")
open(p, "w").write(s)
'

mutate 'the per-address socket cap removed' \
  'src/peer/listener.rs' peer_pairing the_third_unauthenticated_socket_from_one_address_is_refused '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if per_address >= per_address_allowed {"
assert old in s, "the per-address socket cap moved"
s = s.replace(old, "        if false {")
open(p, "w").write(s)
'

mutate 'the caps back to fixed constants, so a granted max_inflight is capped at 2' \
  'src/peer/listener.rs' peer_pairing concurrent_handshakes_up_to_the_granted_inflight_all_complete '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    (\n        MAX_UNAUTHENTICATED_SOCKETS.saturating_add(granted),\n        MAX_UNAUTHENTICATED_PER_ADDRESS.saturating_add(largest),\n    )"
assert old in s, "the allowance derivation moved"
s = s.replace(old, "    let _ = (granted, largest);\n    (MAX_UNAUTHENTICATED_SOCKETS, MAX_UNAUTHENTICATED_PER_ADDRESS)")
open(p, "w").write(s)
'

# The slot's NARROWED scope: released at message 1 rather than when the
# handshake completes: is DEFENCE IN DEPTH WITH NO GATE, and this row records
# that rather than pretending otherwise.
#
# Measured: with the release moved back below `accept_pairing_or_return`, every
# gate in this tree including `borrowed_requests_are_paced_by_the_lenders_own_\
# bucket` stays GREEN. So the narrowing is not what fixes the concurrency
# defect; `unauthenticated_allowance` is, and the row above proves that one.
# The narrowing is kept because the cap's own subject is a connection that has
# said nothing (`abuse-resistance.md`: "5 s to deliver message 1"), and holding
# a slot through a handshake means a stranger who sends message 1 and stalls
# spends HANDSHAKE_TIMEOUT of somebody else's allowance instead of
# MESSAGE_1_TIMEOUT. A gate that cannot be shown failing is not a gate, so this
# is reported as an ungated change and not counted as a proven one.
backup 'src/peer/listener.rs'
MUT_FILE="$ROOT/src/peer/listener.rs" python3 -c '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    drop(slot);\n\n    // Check 4, in one place"
assert old in s, "the slot release moved"
s = s.replace(old, "    // Check 4, in one place")
s = s.replace("        return serve_knock(&mut stream, context, &addr, pattern, &message_1, &file, now)", "        let _ = &slot;\n        return serve_knock(&mut stream, context, &addr, pattern, &message_1, &file, now)")
open(p, "w").write(s)
'
if cargo test --manifest-path "$ROOT/Cargo.toml" --test peer_lease \
     borrowed_requests_are_paced >"$LOG" 2>&1; then
  printf 'src/peer/listener.rs: NOTE: the narrowed slot scope is UNGATED: every gate stays green without it\n'
  printf '  unauthenticated_allowance is what carries the concurrency fix; see the row above\n'
  PASS=$((PASS + 1))
else
  printf 'src/peer/listener.rs: NOTE: the narrowed slot scope now HAS a gate, which is better than recorded\n'
  PASS=$((PASS + 1))
fi
restore_all

mutate 'the pre-allocation length bound removed, so two bytes reserve 65 kB' \
  'src/peer/noise.rs' peer_pairing a_first_frame_above_the_bound_is_refused_before_allocation '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    if len > max_payload {"
assert old in s, "the allocation bound moved"
s = s.replace(old, "    if len > max_payload && false {")
open(p, "w").write(s)
'

mutate 'the found list shows every row, so an announcement flood fills the UI' \
  'src/peer/discovery.rs' peer_discovery the_found_list_shows_twelve_and_counts_the_rest '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "            .take(MAX_FOUND_ROWS)\n"
assert old in s, "the found-row cap moved"
s = s.replace(old, "")
open(p, "w").write(s)
'

mutate 'the per-address found cap removed' \
  'src/peer/discovery.rs' peer_discovery one_address_holds_at_most_two_found_rows '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "            if from_this_address >= MAX_FOUND_PER_ADDRESS {"
assert old in s, "the per-address found cap moved"
s = s.replace(old, "            if false {")
open(p, "w").write(s)
'

mutate 'the refusal log global again, so one flooder silences every address' \
  'src/peer/listener.rs' peer_noise one_flooding_address_does_not_silence_another '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if let Some((last, suppressed)) = self.per_address.get_mut(addr) {"
assert old in s, "the per-address lookup moved"
s = s.replace(old, "        let addr = \"\";\n        if let Some((last, suppressed)) = self.per_address.get_mut(addr) {")
open(p, "w").write(s)
'

# --- Item 11: the network key ------------------------------------------------
mutate 'a knock with no network key admitted at a node that requires one' \
  'src/peer/listener.rs' peer_pairing a_knock_without_the_network_key_gets_zero_bytes '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        (Some(_), Handshake::Knock) => bail!("
assert old in s, "the network-key match moved"
s = s.replace(old, "        (Some(_), Handshake::Knock) if false => bail!(")
open(p, "w").write(s)
'

mutate 'the announcement tag never verified, so an untagged row reaches the UI' \
  'src/peer/discovery.rs' peer_discovery an_untagged_row_never_appears_to_a_mac_with_the_network_key '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    if let Some(key) = network_key {\n        let tag = tag?;"
assert old in s, "the tag verification moved"
s = s.replace(old, "    if let Some(key) = network_key {\n        let _ = &key;\n        let tag = tag.unwrap_or_default();")
s = s.replace("        if !key.tag_matches(tag, &instance_id, port, now_unix_secs) {", "        let _ = tag;\n        if false {")
open(p, "w").write(s)
'

mutate 'the HMAC truncated to the key only, so the tag stops covering port and minute' \
  'src/peer/config.rs' peer_discovery the_announcement_tag_covers_the_instance_the_port_and_the_minute '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        message.extend_from_slice(&port.to_be_bytes());\n        message.extend_from_slice(&minute.to_be_bytes());"
assert old in s, "the tag message assembly moved"
s = s.replace(old, "")
open(p, "w").write(s)
'

mutate 'the HMAC inner/outer pads swapped, so it agrees with nobody' \
  'src/peer/config.rs' peer_pairing hmac_sha256_matches_rfc_4231_case_2 '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "    let mut ipad = [0x36_u8; BLOCK];\n    let mut opad = [0x5c_u8; BLOCK];"
assert old in s, "the HMAC pads moved"
s = s.replace(old, "    let mut ipad = [0x5c_u8; BLOCK];\n    let mut opad = [0x36_u8; BLOCK];")
open(p, "w").write(s)
'

# --- Item 12: the share link -------------------------------------------------
mutate 'the link version not checked, so a v2 link is acted on' \
  'src/peer/pair.rs' peer_pairing a_share_link_round_trips_with_and_without_a_join_key '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "        if version != LINK_VERSION.to_string() {"
assert old in s, "the link version check moved"
s = s.replace(old, "        if false {")
open(p, "w").write(s)
'

mutate 'the network key dropped from a link that also carries a join key' \
  'src/peer/pair.rs' peer_pairing a_link_with_a_spent_join_key_still_sets_the_network_key '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "            Self::Link(link) => Some(link.network_key),"
assert old in s, "JoinInput::network_key moved"
s = s.replace(old, "            Self::Link(link) => link.join.is_none().then_some(link.network_key),")
open(p, "w").write(s)
'

mutate 'the network key set AFTER the enrol attempt, so a dead link sets nothing' \
  'src/main.rs' peer_pairing a_link_with_a_dead_join_key_still_sets_the_network_key_through_the_cli '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "            if let Some(network_key) = input.network_key() {"
assert old in s, "the network-key-first block moved"
s = s.replace(old, "            if false {\n                let _ = input.network_key();\n            }\n            if false {")
open(p, "w").write(s)
'

# --- Item 4: the token never in output ---------------------------------------
mutate 'the join key echoed to stderr on the failure path' \
  'src/main.rs' peer_pairing join_stdin_keeps_the_key_out_of_argv_and_every_log_line '
import os
p = os.environ["MUT_FILE"]
s = open(p).read()
old = "            teamclaude_rs::peer::pair::join(&store, token, &label).await?;"
assert old in s, "the join call moved"
s = s.replace(old, "            eprintln!(\"peer join: dialling {}\", token.to_token());\n            teamclaude_rs::peer::pair::join(&store, token, &label).await?;")
open(p, "w").write(s)
'

printf '\nmutate-pairing-gates: pass=%d fail=%d\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
