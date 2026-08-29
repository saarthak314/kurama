use std::process::Command;

#[test]
fn help_exposes_profiles_resume_and_yolo_without_provider_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_kurama"))
        .arg("--help")
        .output()
        .expect("run kurama --help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 help");
    for flag in ["--profile", "--resume", "--continue", "--yolo"] {
        assert!(stdout.contains(flag), "missing {flag}: {stdout}");
    }
    assert!(!stdout.contains("--provider"));
}
