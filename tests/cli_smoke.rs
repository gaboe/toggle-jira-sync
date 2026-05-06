use std::process::Command;

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

#[test]
fn help_lists_expected_commands() {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("failed to run CLI help");

    assert!(output.status.success(), "help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    for command in ["sync", "recover", "config", "doctor", "status"] {
        assert!(
            stdout.contains(command),
            "expected help to include {command}, got: {stdout}"
        );
    }
}

#[test]
fn unknown_command_fails_gracefully() {
    let output = Command::new(binary())
        .arg("unknown-command")
        .output()
        .expect("failed to run CLI with unknown command");

    assert!(!output.status.success(), "unknown command should fail");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized subcommand"),
        "stderr was: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "stderr was: {stderr}");
}
