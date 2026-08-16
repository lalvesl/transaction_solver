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

/// Runs `<name>.csv` and compares stdout with its golden file.
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
        stdout_of(&output),
        expected,
        "{name} does not match {}",
        golden.display()
    );

    output
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
fn the_input_path_is_required() {
    let output = Command::new(env!("CARGO_BIN_EXE_transaction_solver"))
        .output()
        .expect("the binary should be runnable");

    assert!(!output.status.success());
    assert!(stdout_of(&output).is_empty());
}
