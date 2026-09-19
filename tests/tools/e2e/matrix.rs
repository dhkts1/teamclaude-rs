//! One line per E2E-MATRIX scenario, printed the same way `step()` in
//! `tests/peer_e2e.rs` prints one line per driver step.
//!
//! `scripts/peer-e2e-local.sh` (owned by the reverse-dial suite) and a human running
//! `cargo test --test peer_e2e -- --nocapture` read the same stdout, so this
//! is the one place the line's shape is written rather than four scenarios
//! each choosing their own wording.

/// `MATRIX: <scenario> -> <outcome>`, where `outcome` is one line of prose
/// naming what the scenario proved or why it could not run.
///
/// A function rather than a `println!` copied into every scenario: four
/// copies of the same `format!` are four chances for one of them to drop the
/// `MATRIX:` marker a watcher greps for.
pub fn matrix_line(scenario: &str, outcome: &str) -> String {
    format!("MATRIX: {scenario} -> {outcome}")
}

#[cfg(test)]
mod tests {
    use super::matrix_line;

    #[test]
    fn matrix_line_carries_the_grep_marker_and_both_fields() {
        let line = matrix_line("no_ipv6", "forwarder fallback served the borrow");
        assert!(
            line.starts_with("MATRIX: "),
            "a watcher greps for the literal marker: {line:?}"
        );
        assert!(
            line.contains("no_ipv6") && line.contains("forwarder fallback served the borrow"),
            "both the scenario name and its outcome must survive: {line:?}"
        );
    }
}
