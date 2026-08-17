//! End-to-end tests: the real binary, the sample files in `tests/data`, and committed
//! golden outputs.
//!
//! Each `<name>.csv` is paired with `<name>.expected`. A case whose result depends on the
//! `dispute-withdraw` feature also carries a `<name>.expected-dispute-withdraw`.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn solve(input: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
        .arg(input)
        .output()
        .expect("the binary should be runnable")
}

fn stdout_of(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("output should be UTF-8")
}

fn stderr_of(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("diagnostics should be UTF-8")
}

/// The header and the data rows of a CSV, with the rows sorted.
///
/// Accounts are written as they finish, and shards finish concurrently, so row order is
/// whatever the scheduler produced on the day. The specification says row order is
/// irrelevant; what has to hold is that the same set of accounts comes out with the same
/// numbers. Sorting here compares exactly that, and nothing that is not a guarantee.
#[track_caller]
fn rows(csv: &str) -> (String, Vec<String>) {
    let mut lines = csv.lines();
    let header = lines.next().unwrap_or_default().to_owned();
    let mut rows: Vec<String> = lines
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    rows.sort();
    (header, rows)
}

/// Runs `<name>.csv` and compares stdout with its golden file, as a set of rows.
#[track_caller]
fn check(name: &str) -> Output {
    let input = data_dir().join(format!("{name}.csv"));

    let variant = data_dir().join(format!("{name}.expected-dispute-withdraw"));
    let golden = if cfg!(feature = "dispute-withdraw") && variant.exists() {
        variant
    } else {
        data_dir().join(format!("{name}.expected"))
    };

    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", golden.display()));

    let output = solve(&input);
    assert!(
        output.status.success(),
        "{name} should exit successfully, stderr:\n{}",
        stderr_of(&output)
    );
    assert_eq!(
        rows(stdout_of(&output)),
        rows(&expected),
        "{name} does not match {}",
        golden.display()
    );

    output
}

/// Runs a case at several shard counts and requires the same answer from each.
///
/// One shard is the sharding switched off; the others split the same clients different
/// ways. A rule that only holds when a client's records all land on one engine would show
/// up here as a disagreement.
#[track_caller]
fn check_across_shards(name: &str) {
    let input = data_dir().join(format!("{name}.csv"));

    let baseline = Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
        .arg("--shards")
        .arg("1")
        .arg(&input)
        .output()
        .expect("the binary should be runnable");
    let expected = rows(stdout_of(&baseline));

    for shards in ["2", "3", "7", "16"] {
        let output = Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
            .arg("--shards")
            .arg(shards)
            .arg(&input)
            .output()
            .expect("the binary should be runnable");

        assert!(
            output.status.success(),
            "{name} at {shards} shards should exit successfully, stderr:\n{}",
            stderr_of(&output)
        );
        assert_eq!(
            rows(stdout_of(&output)),
            expected,
            "{name} at {shards} shards disagrees with the single-shard run"
        );
    }
}

#[test]
fn specification_example() {
    check("spec_example");
}

#[test]
fn whitespace_and_missing_columns_are_tolerated() {
    let output = check("whitespace");
    assert!(
        stderr_of(&output).is_empty(),
        "nothing should have been rejected: {}",
        stderr_of(&output)
    );
}

#[test]
fn disputes_resolutions_and_chargebacks() {
    check("dispute_lifecycle");
}

#[test]
fn a_reversal_after_the_funds_were_spent_leaves_a_negative_balance() {
    check("reversal_after_spend");
}

#[test]
fn four_decimal_places_survive_a_round_trip() {
    check("precision");
}

#[test]
fn a_locked_account_strands_its_open_disputes() {
    check("locked_account");
}

#[test]
fn withdrawal_disputes_follow_the_feature() {
    check("withdrawal_dispute");
}

#[test]
fn partner_errors_are_reported_but_do_not_stop_the_run() {
    let output = check("partner_errors");

    let diagnostics = stderr_of(&output);
    assert!(
        diagnostics.contains("12 record(s) rejected"),
        "unexpected diagnostics:\n{diagnostics}"
    );

    for expected in [
        "already has a transaction",
        "unrecognised transaction type `transfer`",
        "more than four decimal places",
        "is negative",
        "requires an amount",
        "client 1 has 10.0 available, cannot withdraw 999.0",
        "no disputable transaction",
        "is not under dispute",
        "not a valid decimal",
    ] {
        assert!(
            diagnostics.contains(expected),
            "expected {expected:?} in:\n{diagnostics}"
        );
    }
}

#[test]
fn stdout_stays_clean_csv_while_records_are_rejected() {
    let output = solve(&data_dir().join("partner_errors.csv"));

    // Every line of stdout must be a five-field CSV row and nothing else.
    for line in stdout_of(&output).lines() {
        assert_eq!(
            line.split(',').count(),
            5,
            "stdout should carry only CSV rows, found: {line}"
        );
    }
    assert!(
        !stderr_of(&output).is_empty(),
        "diagnostics belong on stderr"
    );
}

#[test]
fn a_missing_input_file_is_fatal() {
    let output = solve(&data_dir().join("does_not_exist.csv"));

    assert!(!output.status.success());
    assert!(stdout_of(&output).is_empty(), "no output on failure");
    assert!(
        stderr_of(&output).contains("cannot open"),
        "unexpected diagnostics: {}",
        stderr_of(&output)
    );
}

#[test]
fn reads_from_stdin_when_no_argument_given() {
    let input_bytes = std::fs::read(data_dir().join("spec_example.csv")).expect("read sample");
    let expected =
        std::fs::read_to_string(data_dir().join("spec_example.expected")).expect("read expected");

    let mut child = Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary should be runnable");

    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin handle");
        stdin.write_all(&input_bytes).expect("write to stdin");
    }

    let output = child.wait_with_output().expect("wait for output");
    assert!(
        output.status.success(),
        "should exit successfully, stderr:\n{}",
        stderr_of(&output)
    );
    assert_eq!(rows(stdout_of(&output)), rows(&expected));
}

#[test]
fn reads_from_stdin_when_dash_argument_given() {
    let input_bytes = std::fs::read(data_dir().join("spec_example.csv")).expect("read sample");
    let expected =
        std::fs::read_to_string(data_dir().join("spec_example.expected")).expect("read expected");

    let mut child = Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary should be runnable");

    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin handle");
        stdin.write_all(&input_bytes).expect("write to stdin");
    }

    let output = child.wait_with_output().expect("wait for output");
    assert!(
        output.status.success(),
        "should exit successfully, stderr:\n{}",
        stderr_of(&output)
    );
    assert_eq!(rows(stdout_of(&output)), rows(&expected));
}

/// The property that makes sharding legitimate: how the clients are split cannot change
/// the answer. Every sample, at five different splits.
#[test]
fn the_shard_count_never_changes_the_result() {
    for name in [
        "spec_example",
        "dispute_lifecycle",
        "locked_account",
        "partner_errors",
        "precision",
        "reversal_after_spend",
        "whitespace",
        "withdrawal_dispute",
    ] {
        check_across_shards(name);
    }
}

/// Every client is reported exactly once, whether its account froze mid-run and was
/// evicted or survived to the drain at the end. Getting this wrong is the obvious way an
/// eviction scheme breaks: the row goes out twice, or not at all.
#[test]
fn a_frozen_client_is_reported_once_and_only_once() {
    let output = solve(&data_dir().join("locked_account.csv"));
    let (_, written) = rows(stdout_of(&output));

    let mut clients: Vec<&str> = written
        .iter()
        .map(|row| row.split(',').next().unwrap_or_default())
        .collect();
    let before = clients.len();
    clients.sort_unstable();
    clients.dedup();

    assert_eq!(
        before,
        clients.len(),
        "a client was written twice: {written:?}"
    );
    assert!(
        written.iter().any(|row| row.ends_with(",true")),
        "this sample is meant to freeze an account: {written:?}"
    );
}
