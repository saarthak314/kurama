use kurama_cli::args::{ResumeChoice, parse_from};

#[test]
fn resume_subcommand_alias_matches_resume_flag() {
    let alias = parse_from(["resume", "session-123"]).expect("parse resume alias");
    let flag = parse_from(["--resume", "session-123"]).expect("parse resume flag");

    assert_eq!(alias.resume, Some(ResumeChoice::Id("session-123".into())));
    assert_eq!(alias, flag);
}

#[test]
fn resume_subcommand_requires_an_id() {
    let alias_error = parse_from(["resume"]).expect_err("resume alias should require an id");

    assert!(
        alias_error.contains("missing argument"),
        "unexpected error: {alias_error}"
    );
}
