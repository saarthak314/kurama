use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Component, Path, PathBuf},
};

use kurama_protocol::{
    agent::WriteScope,
    policy::{AutoBoundaries, ExecutionMode, PolicyContext, PolicyDecision},
    tool::{CommandClass, Operation, split_shell_commands},
    traits::ApprovalPolicy,
};

#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    mode: ExecutionMode,
}

impl DefaultPolicy {
    pub fn new(mode: ExecutionMode, _boundaries: AutoBoundaries) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    fn supervised(&self, context: &PolicyContext, operation: &Operation) -> PolicyDecision {
        match operation {
            Operation::Read { path, external } => {
                if !external && within_workspace(path, context) {
                    PolicyDecision::Allow
                } else {
                    ask("read is outside the workspace boundary")
                }
            }
            Operation::Write {
                paths, external, ..
            } => {
                if let Some(reason) = write_scope_violation(paths, context) {
                    deny(reason)
                } else if *external || paths.iter().any(|path| !within_workspace(path, context)) {
                    ask("write reaches outside the workspace")
                } else {
                    ask("write requires approval in supervised mode")
                }
            }
            Operation::Bash {
                command,
                cwd,
                class,
                ..
            } => {
                if *class == CommandClass::ReadOnly
                    && within_workspace(cwd, context)
                    && supervised_command_allowed(command, cwd, context)
                {
                    PolicyDecision::Allow
                } else if *class != CommandClass::ReadOnly && context.write_scope.is_read_only() {
                    deny("child write scope is read-only")
                } else {
                    ask("command is not on the supervised read-only allowlist")
                }
            }
            Operation::WebSearch {
                contains_workspace_data,
                ..
            } => {
                if *contains_workspace_data {
                    ask("search contains workspace data")
                } else {
                    PolicyDecision::Allow
                }
            }
            Operation::WebOpen {
                url,
                private_target,
            } => {
                if !private_target && public_http_url(url).is_some() {
                    PolicyDecision::Allow
                } else {
                    ask("URL is private or not public HTTP(S)")
                }
            }
        }
    }

    fn automatic(&self, context: &PolicyContext, operation: &Operation) -> PolicyDecision {
        match operation {
            Operation::Read { path, external } => {
                if !external && within_workspace(path, context) {
                    PolicyDecision::Allow
                } else {
                    deny("read exceeds the automatic workspace boundary")
                }
            }
            Operation::Write {
                paths, external, ..
            } => {
                if let Some(reason) = write_scope_violation(paths, context) {
                    return deny(reason);
                }
                if *external
                    || paths
                        .iter()
                        .any(|path| !within_any(path, &context.auto.write_roots, context))
                {
                    deny("write exceeds configured automatic roots")
                } else {
                    PolicyDecision::Allow
                }
            }
            Operation::Bash {
                command,
                cwd,
                class,
                ..
            } => {
                let Some(segments) = split_shell_commands(command) else {
                    return deny("command uses shell composition");
                };
                if !within_workspace(cwd, context) {
                    return deny("command working directory is outside the workspace");
                }
                if *class != CommandClass::ReadOnly && context.write_scope.is_read_only() {
                    return deny("child write scope is read-only");
                }
                if segments.into_iter().all(|segment| {
                    safe_split(segment)
                        .and_then(|tokens| {
                            tokens
                                .first()
                                .and_then(|value| Path::new(value).file_name())
                                .and_then(|value| value.to_str())
                                .map(str::to_owned)
                        })
                        .is_some_and(|name| {
                            context
                                .auto
                                .allowed_commands
                                .iter()
                                .any(|allowed| allowed == &name)
                        })
                }) {
                    PolicyDecision::Allow
                } else {
                    deny("command is not in the automatic allowlist")
                }
            }
            Operation::WebSearch {
                contains_workspace_data,
                ..
            } => {
                if *contains_workspace_data {
                    deny("automatic search cannot transmit workspace data")
                } else {
                    PolicyDecision::Allow
                }
            }
            Operation::WebOpen {
                url,
                private_target,
            } => {
                let host = public_http_url(url);
                if !private_target
                    && host.is_some_and(|host| {
                        context
                            .auto
                            .allowed_hosts
                            .iter()
                            .any(|allowed| host_matches(&host, allowed))
                    })
                {
                    PolicyDecision::Allow
                } else {
                    deny("host is not in the automatic allowlist")
                }
            }
        }
    }
}

impl ApprovalPolicy for DefaultPolicy {
    fn decide(&self, context: &PolicyContext, operation: &Operation) -> PolicyDecision {
        match context.mode {
            ExecutionMode::Supervised => self.supervised(context, operation),
            ExecutionMode::Auto => self.automatic(context, operation),
            ExecutionMode::Yolo => PolicyDecision::Allow,
        }
    }
}

fn ask(reason: impl Into<String>) -> PolicyDecision {
    PolicyDecision::Ask {
        reason: reason.into(),
    }
}

fn deny(reason: impl Into<String>) -> PolicyDecision {
    PolicyDecision::Deny {
        reason: reason.into(),
    }
}

fn within_workspace(path: &Path, context: &PolicyContext) -> bool {
    contained(path, &context.workspace_root, &context.workspace_root)
}

fn within_any(path: &Path, roots: &[PathBuf], context: &PolicyContext) -> bool {
    roots
        .iter()
        .any(|root| contained(path, root, &context.workspace_root))
}

fn write_scope_violation(paths: &[PathBuf], context: &PolicyContext) -> Option<&'static str> {
    if context.write_scope.is_read_only() {
        return Some("child write scope is read-only");
    }
    if paths
        .iter()
        .all(|path| scope_contains(path, &context.write_scope, &context.workspace_root))
    {
        None
    } else {
        Some("write exceeds the child write scope")
    }
}

fn scope_contains(path: &Path, scope: &WriteScope, workspace: &Path) -> bool {
    scope
        .roots
        .iter()
        .any(|root| contained(path, root, workspace))
        || scope.files.iter().any(|file| {
            canonical_candidate(path, workspace)
                .zip(canonical_candidate(file, workspace))
                .is_some_and(|(path, file)| path == file)
        })
}

fn contained(path: &Path, root: &Path, workspace: &Path) -> bool {
    canonical_candidate(path, workspace)
        .zip(canonical_candidate(root, workspace))
        .is_some_and(|(path, root)| path.starts_with(root))
}

fn canonical_candidate(path: &Path, base: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    if absolute.exists() {
        return absolute.canonicalize().ok();
    }

    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name()?.to_owned();
        suffix.push(name);
        existing = existing.parent()?;
    }
    let mut resolved = existing.canonicalize().ok()?;
    for component in suffix.iter().rev() {
        let component = Path::new(component).components().next()?;
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
        resolved.push(component.as_os_str());
    }
    Some(resolved)
}

fn safe_split(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut active = false;
    for character in command.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            active = true;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    token.push(character);
                }
                active = true;
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    escaped = true;
                } else {
                    token.push(character);
                }
                active = true;
            }
            Some(_) => return None,
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                active = true;
            }
            None if character == '\\' => {
                escaped = true;
                active = true;
            }
            None if character.is_whitespace() => {
                if active {
                    tokens.push(std::mem::take(&mut token));
                    active = false;
                }
            }
            None => {
                token.push(character);
                active = true;
            }
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if active {
        tokens.push(token);
    }
    Some(tokens)
}

fn supervised_command_allowed(command: &str, cwd: &Path, context: &PolicyContext) -> bool {
    let Some(segments) = split_shell_commands(command) else {
        return false;
    };
    segments.into_iter().all(|segment| {
        let Some(tokens) = safe_split(segment) else {
            return false;
        };
        supervised_segment_allowed(&tokens, cwd, context)
    })
}

fn supervised_segment_allowed(tokens: &[String], cwd: &Path, context: &PolicyContext) -> bool {
    let Some(executable) = tokens.first().map(String::as_str) else {
        return false;
    };
    let command_allowed = match executable {
        "basename" | "cat" | "dirname" | "file" | "grep" | "head" | "printf" | "pwd"
        | "realpath" | "stat" | "tail" | "uniq" | "wc" => true,
        "ls" => !tokens
            .iter()
            .skip(1)
            .any(|token| token == "--dereference-command-line-symlink-to-dir"),
        "rg" => !tokens.iter().any(|token| {
            token == "--replace"
                || token.starts_with("--replace=")
                || token == "--pre"
                || token.starts_with("--pre=")
        }),
        "find" => find_is_read_only(tokens),
        "sed" => sed_is_read_only(tokens),
        "sort" => !tokens
            .iter()
            .skip(1)
            .any(|token| token == "-o" || token.starts_with("--output=")),
        "git" => git_is_read_only(tokens),
        _ => false,
    };
    command_allowed && path_arguments_stay_inside(tokens, cwd, context)
}

fn find_is_read_only(tokens: &[String]) -> bool {
    !tokens.iter().skip(1).any(|token| {
        matches!(
            token.as_str(),
            "-delete" | "-exec" | "-execdir" | "-fprint" | "-fprint0" | "-fls" | "-ok" | "-okdir"
        )
    })
}

fn sed_is_read_only(tokens: &[String]) -> bool {
    if tokens.len() < 2
        || tokens
            .iter()
            .any(|token| token == "-i" || token.starts_with("-i") || token == "--in-place")
    {
        return false;
    }
    tokens.iter().skip(1).all(|token| {
        token == "-n"
            || token == "--quiet"
            || token == "--silent"
            || token == "-e"
            || token.starts_with('-')
            || token.ends_with('p')
            || token.ends_with(".rs")
            || token.contains('/')
            || Path::new(token).exists()
    })
}

fn git_is_read_only(tokens: &[String]) -> bool {
    matches!(
        tokens.get(1).map(String::as_str),
        Some("diff" | "grep" | "log" | "ls-files" | "ls-tree" | "rev-parse" | "show" | "status")
    ) && !tokens.iter().any(|token| {
        token == "-c"
            || token.starts_with("--config-env")
            || token == "--ext-diff"
            || token == "--textconv"
    })
}

fn path_arguments_stay_inside(tokens: &[String], cwd: &Path, context: &PolicyContext) -> bool {
    tokens.iter().skip(1).all(|token| {
        if token.starts_with('-') || !looks_like_path(token, cwd) {
            return true;
        }
        let path = Path::new(token);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            cwd.join(path)
        };
        within_workspace(&path, context)
    })
}

fn looks_like_path(value: &str, cwd: &Path) -> bool {
    let path = Path::new(value);
    path.is_absolute() || value.starts_with('.') || value.contains('/') || cwd.join(path).exists()
}

fn public_http_url(value: &str) -> Option<String> {
    let remainder = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let authority = remainder.split(['/', '?', '#']).next()?.trim();
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']')?;
        if !bracketed[end + 1..].is_empty()
            && !bracketed[end + 1..].strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.chars().all(|digit| digit.is_ascii_digit())
            })
        {
            return None;
        }
        &bracketed[..end]
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':')
            || port.is_empty()
            || !port.chars().all(|digit| digit.is_ascii_digit())
        {
            return None;
        }
        host
    } else {
        authority
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || (!host.contains('.') && host.parse::<IpAddr>().is_err())
    {
        return None;
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_private_ip(ip)
    {
        return None;
    }
    Some(host)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 224
                || ip == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip == Ipv6Addr::LOCALHOST
        }
    }
}

fn host_matches(host: &str, allowed: &str) -> bool {
    let allowed = allowed.trim_end_matches('.').to_ascii_lowercase();
    host == allowed || host.ends_with(&format!(".{allowed}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurama_protocol::agent::WriteScope;

    #[test]
    fn decision_matrix_has_no_prompts_in_auto_or_yolo() {
        let workspace = std::env::current_dir().expect("cwd");
        let auto = AutoBoundaries {
            write_roots: vec![workspace.clone()],
            allowed_commands: vec!["cargo".into()],
            allowed_hosts: vec!["docs.rs".into()],
        };
        let context = PolicyContext {
            mode: ExecutionMode::Auto,
            workspace_root: workspace.clone(),
            write_scope: WriteScope {
                roots: vec![workspace.clone()],
                files: Vec::new(),
            },
            auto: auto.clone(),
        };
        let operations = [
            Operation::Read {
                path: workspace.join("Cargo.toml"),
                external: false,
            },
            Operation::Write {
                paths: vec![workspace.join("new")],
                destructive: false,
                external: false,
            },
            Operation::WebOpen {
                url: "https://docs.rs".into(),
                private_target: false,
            },
        ];
        for mode in [ExecutionMode::Auto, ExecutionMode::Yolo] {
            let policy = DefaultPolicy::new(mode, auto.clone());
            assert!(operations.iter().all(|operation| {
                !matches!(
                    policy.decide(&context, operation),
                    PolicyDecision::Ask { .. }
                )
            }));
        }
    }
}
