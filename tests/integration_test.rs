//! Integration tests that drive the real `tidocs` binary via `assert_cmd`.
//!
//! Uses the non-interactive `--query` mode which searches and prints results
//! to stdout without needing a TTY.

use assert_cmd::Command;

/// Return a `Command` pointing at the compiled `tidocs` binary.
fn tidocs_cmd() -> Command {
    Command::cargo_bin("tidocs").unwrap()
}

/// Search for `ratatui::text::Span::add` and verify that "fn add" appears in the output.
///
/// This test is expected to **fail** right now because no ratatui doc source
/// is indexed in the local registry — the search returns no results.
#[test]
fn search_span_add_shows_fn_add() {
    let mut cmd = tidocs_cmd();

    // Use --query to search, --details to print full doc of first match.
    let output = cmd
        .args(["--query", "ratatui::text::Span::add", "--details"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("fn add"),
        "Expected 'fn add' in output, but got:\n{}",
        stdout
    );
}
