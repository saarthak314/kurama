use std::path::PathBuf;

use kurama_core::policy::DefaultPolicy;
use kurama_protocol::{
    agent::WriteScope,
    policy::{AutoBoundaries, ExecutionMode, PolicyContext, PolicyDecision},
    tool::{CommandClass, Operation},
    traits::ApprovalPolicy,
};

fn context(mode: ExecutionMode) -> PolicyContext {
    let workspace_root = std::env::current_dir().expect("current dir");
    PolicyContext {
        mode,
        workspace_root: workspace_root.clone(),
        write_scope: WriteScope {
            roots: vec![workspace_root.clone()],
            files: Vec::new(),
        },
        auto: AutoBoundaries {
            write_roots: vec![workspace_root],
            allowed_commands: vec!["cargo".into()],
            allowed_hosts: vec!["docs.rs".into()],
        },
    }
}

fn read(path: PathBuf) -> Operation {
    Operation::Read {
        path,
        external: false,
    }
}

#[test]
fn supervised_allows_internal_reads_and_prompts_for_writes() {
    let context = context(ExecutionMode::Supervised);
    let policy = DefaultPolicy::new(ExecutionMode::Supervised, context.auto.clone());
    assert_eq!(
        policy.decide(&context, &read(context.workspace_root.join("Cargo.toml"))),
        PolicyDecision::Allow
    );
    assert!(matches!(
        policy.decide(
            &context,
            &Operation::Write {
                paths: vec![context.workspace_root.join("new.txt")],
                destructive: false,
                external: false,
            }
        ),
        PolicyDecision::Ask { .. }
    ));
    assert!(matches!(
        policy.decide(&context, &read(context.workspace_root.join("../outside"))),
        PolicyDecision::Ask { .. }
    ));
}

#[test]
fn auto_denies_boundary_expansion() {
    let context = context(ExecutionMode::Auto);
    let policy = DefaultPolicy::new(ExecutionMode::Auto, context.auto.clone());
    assert_eq!(
        policy.decide(
            &context,
            &Operation::Write {
                paths: vec![context.workspace_root.join("new.txt")],
                destructive: false,
                external: false,
            }
        ),
        PolicyDecision::Allow
    );
    assert!(matches!(
        policy.decide(
            &context,
            &Operation::WebOpen {
                url: "https://unlisted.example/docs".into(),
                private_target: false,
            }
        ),
        PolicyDecision::Deny { .. }
    ));
}

#[test]
fn auto_checks_every_composed_command_against_the_allowlist() {
    let mut context = context(ExecutionMode::Auto);
    context.auto.allowed_commands = vec!["pwd".into(), "rg".into(), "sed".into()];
    let policy = DefaultPolicy::new(ExecutionMode::Auto, context.auto.clone());
    let bash = |command: &str, class| Operation::Bash {
        command: command.into(),
        cwd: context.workspace_root.clone(),
        class,
        timeout_ms: 1_000,
    };

    assert_eq!(
        policy.decide(
            &context,
            &bash("pwd && rg --files | sed -n '1,20p'", CommandClass::ReadOnly)
        ),
        PolicyDecision::Allow
    );
    assert!(matches!(
        policy.decide(&context, &bash("pwd && rm -rf .", CommandClass::Mutating)),
        PolicyDecision::Deny { .. }
    ));
}

#[test]
fn yolo_allows_every_classified_operation() {
    let context = context(ExecutionMode::Yolo);
    let policy = DefaultPolicy::new(ExecutionMode::Yolo, AutoBoundaries::default());
    let operations = [
        read(PathBuf::from("/private/outside")),
        Operation::Bash {
            command: "rm -rf /".into(),
            cwd: PathBuf::from("/"),
            class: CommandClass::Mutating,
            timeout_ms: 1_000,
        },
        Operation::WebOpen {
            url: "http://127.0.0.1".into(),
            private_target: true,
        },
    ];
    assert!(
        operations
            .iter()
            .all(|operation| policy.decide(&context, operation) == PolicyDecision::Allow)
    );
}

#[test]
fn supervised_bash_allowlist_accepts_composed_reads_and_rejects_mutation() {
    let context = context(ExecutionMode::Supervised);
    let policy = DefaultPolicy::new(ExecutionMode::Supervised, context.auto.clone());
    let bash = |command: &str| Operation::Bash {
        command: command.into(),
        cwd: context.workspace_root.clone(),
        class: CommandClass::ReadOnly,
        timeout_ms: 1_000,
    };
    assert_eq!(
        policy.decide(&context, &bash("git status --short")),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.decide(&context, &bash("pwd && rg --files | sed -n '1,240p'")),
        PolicyDecision::Allow
    );
    assert!(matches!(
        policy.decide(&context, &bash("git status; rm -rf .")),
        PolicyDecision::Ask { .. }
    ));
    assert!(matches!(
        policy.decide(&context, &bash("sed -i '' s/a/b/ file")),
        PolicyDecision::Ask { .. }
    ));
}

#[test]
fn read_only_child_cannot_write_outside_its_scope() {
    let mut context = context(ExecutionMode::Auto);
    context.write_scope = WriteScope::default();
    let policy = DefaultPolicy::new(ExecutionMode::Auto, context.auto.clone());
    assert!(matches!(
        policy.decide(
            &context,
            &Operation::Write {
                paths: vec![context.workspace_root.join("new.txt")],
                destructive: false,
                external: false,
            }
        ),
        PolicyDecision::Deny { .. }
    ));
}

#[test]
fn policy_follows_the_runtime_context_mode() {
    let mut context = context(ExecutionMode::Supervised);
    let policy = DefaultPolicy::new(ExecutionMode::Supervised, context.auto.clone());
    let operation = Operation::Write {
        paths: vec![context.workspace_root.join("new.txt")],
        destructive: false,
        external: false,
    };
    assert!(matches!(
        policy.decide(&context, &operation),
        PolicyDecision::Ask { .. }
    ));
    context.mode = ExecutionMode::Auto;
    assert_eq!(policy.decide(&context, &operation), PolicyDecision::Allow);
}
