use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use kurama_protocol::{
    KuramaError,
    agent::{
        AgentBudget, AgentSnapshot, AgentSpec, DelegationRequest, OrchestrationContext,
        ResolvedAgentSpec, SchedulePlan, WriteScope,
    },
    id::{AgentId, CallId, OperationId, SessionId},
    traits::{IdGenerator, Orchestrator},
};

pub struct SmartOrchestrator {
    ids: Arc<dyn IdGenerator>,
}

impl SmartOrchestrator {
    pub fn new(ids: Arc<dyn IdGenerator>) -> Self {
        Self { ids }
    }

    fn validate_request(
        &self,
        request: &DelegationRequest,
        context: &OrchestrationContext,
    ) -> Result<(), KuramaError> {
        if context.depth >= 1 {
            return Err(KuramaError::Protocol(
                "maximum sub-agent depth is one".into(),
            ));
        }
        if request.agents.is_empty() || request.agents.len() > 8 {
            return Err(KuramaError::Protocol(
                "delegation must contain between one and eight agents".into(),
            ));
        }

        let mut keys = BTreeSet::new();
        let mut roles: BTreeMap<&str, usize> = BTreeMap::new();
        let mut objectives: BTreeMap<&str, usize> = BTreeMap::new();
        for spec in &request.agents {
            let key = agent_key(spec);
            if !keys.insert(key) {
                return Err(KuramaError::Protocol(
                    "delegation contains duplicate role/objective pairs".into(),
                ));
            }
            *roles.entry(&spec.role).or_default() += 1;
            *objectives.entry(&spec.objective).or_default() += 1;
            validate_budget(&spec.budget)?;
            if !context.yolo && !scope_is_subset(&spec.write_scope, &context.parent_write_scope) {
                return Err(KuramaError::Policy(format!(
                    "agent {} write scope exceeds the parent scope",
                    spec.role
                )));
            }
        }

        for spec in &request.agents {
            for dependency in &spec.depends_on {
                if !dependency_exists(dependency, &keys, &roles, &objectives) {
                    return Err(KuramaError::Protocol(format!(
                        "unknown dependency {dependency} for {}",
                        spec.role
                    )));
                }
            }
        }
        ensure_acyclic(&request.agents, &roles, &objectives)
    }

    fn resolve_spec(
        &self,
        spec: AgentSpec,
        context: &OrchestrationContext,
    ) -> Result<ResolvedAgentSpec, KuramaError> {
        let profile_name = context
            .role_routes
            .get(&spec.role)
            .cloned()
            .unwrap_or_else(|| context.parent_profile.name.clone());
        let profile = context
            .profiles
            .get(&profile_name)
            .cloned()
            .or_else(|| {
                (context.parent_profile.name == profile_name)
                    .then(|| context.parent_profile.clone())
            })
            .ok_or_else(|| KuramaError::Configuration(format!("unknown profile {profile_name}")))?;
        if spec.budget.max_input_tokens > profile.max_input_tokens
            || spec.budget.max_output_tokens > profile.max_output_tokens
        {
            return Err(KuramaError::Configuration(format!(
                "agent {} budget exceeds profile limits",
                spec.role
            )));
        }
        let mut escalation_profiles = context
            .role_escalations
            .get(&spec.role)
            .cloned()
            .unwrap_or_default();
        escalation_profiles.extend(
            context
                .profile_escalations
                .get(&profile_name)
                .cloned()
                .unwrap_or_default(),
        );
        escalation_profiles.retain(|candidate| {
            candidate != &profile_name && context.profiles.contains_key(candidate)
        });
        escalation_profiles.dedup();

        Ok(ResolvedAgentSpec {
            id: self.ids.agent_id(),
            parent_id: None,
            depth: 1,
            role: spec.role,
            objective: spec.objective,
            profile,
            context_refs: spec.context_refs,
            write_scope: canonical_scope(spec.write_scope),
            budget: spec.budget,
            depends_on: spec.depends_on,
            escalation_profiles,
        })
    }
}

impl Default for SmartOrchestrator {
    fn default() -> Self {
        Self::new(Arc::new(LocalIds::default()))
    }
}

impl Orchestrator for SmartOrchestrator {
    fn explicit_delegation(&self, user_text: &str) -> bool {
        let normalized: String = user_text
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character.to_ascii_lowercase()
                } else {
                    ' '
                }
            })
            .collect();
        let words: Vec<_> = normalized.split_whitespace().collect();
        words
            .windows(2)
            .any(|pair| matches!(pair, ["sub", "agent"] | ["use", "agents"]))
            || words.iter().any(|word| {
                matches!(
                    *word,
                    "sub-agent"
                        | "sub-agents"
                        | "subagent"
                        | "subagents"
                        | "delegate"
                        | "delegates"
                        | "parallelize"
                        | "parallelise"
                )
            })
            || normalized.contains("split this between")
    }

    fn resolve(
        &self,
        mut request: DelegationRequest,
        context: &OrchestrationContext,
    ) -> Result<SchedulePlan, KuramaError> {
        for spec in &mut request.agents {
            spec.role = infer_role(&spec.objective, context);
            spec.profile = None;
        }
        self.validate_request(&request, context)?;
        let max_concurrency = context.max_concurrency.clamp(1, 8);
        let mut ready = Vec::new();
        let mut queued = Vec::new();
        let mut blocked = Vec::new();
        let mut active_scopes = Vec::new();

        for spec in request.agents {
            let resolved = self.resolve_spec(spec, context)?;
            if !resolved.depends_on.is_empty() {
                blocked.push(resolved);
            } else if ready.len() < max_concurrency
                && active_scopes
                    .iter()
                    .all(|scope| !scopes_overlap(scope, &resolved.write_scope))
            {
                active_scopes.push(resolved.write_scope.clone());
                ready.push(resolved);
            } else {
                queued.push(resolved);
            }
        }
        Ok(SchedulePlan {
            ready,
            queued,
            blocked,
        })
    }

    fn escalate(
        &self,
        agent: &AgentSnapshot,
        _reason: &str,
        context: &OrchestrationContext,
    ) -> Result<Option<String>, KuramaError> {
        let mut candidates = context
            .role_escalations
            .get(&agent.role)
            .cloned()
            .unwrap_or_default();
        candidates.extend(
            context
                .profile_escalations
                .get(&agent.profile)
                .cloned()
                .unwrap_or_default(),
        );
        Ok(candidates.into_iter().find(|candidate| {
            candidate != &agent.profile && context.profiles.contains_key(candidate)
        }))
    }
}

fn validate_budget(budget: &AgentBudget) -> Result<(), KuramaError> {
    if budget.max_input_tokens == 0
        || budget.max_output_tokens == 0
        || budget.max_turns == 0
        || budget.max_seconds == 0
    {
        return Err(KuramaError::Protocol(
            "agent budgets must be positive".into(),
        ));
    }
    let maxima = AgentBudget::default();
    if budget.max_input_tokens > maxima.max_input_tokens
        || budget.max_output_tokens > maxima.max_output_tokens
        || budget.max_turns > maxima.max_turns
        || budget.max_seconds > maxima.max_seconds
    {
        return Err(KuramaError::Protocol(
            "agent budget exceeds configured core maxima".into(),
        ));
    }
    Ok(())
}

fn agent_key(spec: &AgentSpec) -> String {
    format!("{}:{}", spec.role, spec.objective)
}

fn dependency_exists(
    dependency: &str,
    keys: &BTreeSet<String>,
    roles: &BTreeMap<&str, usize>,
    objectives: &BTreeMap<&str, usize>,
) -> bool {
    keys.contains(dependency)
        || roles.get(dependency).is_some_and(|count| *count == 1)
        || objectives.get(dependency).is_some_and(|count| *count == 1)
}

fn dependency_index(
    dependency: &str,
    specs: &[AgentSpec],
    roles: &BTreeMap<&str, usize>,
    objectives: &BTreeMap<&str, usize>,
) -> Option<usize> {
    specs.iter().position(|spec| {
        agent_key(spec) == dependency
            || (spec.role == dependency && roles.get(spec.role.as_str()) == Some(&1))
            || (spec.objective == dependency && objectives.get(spec.objective.as_str()) == Some(&1))
    })
}

fn ensure_acyclic(
    specs: &[AgentSpec],
    roles: &BTreeMap<&str, usize>,
    objectives: &BTreeMap<&str, usize>,
) -> Result<(), KuramaError> {
    fn visit(
        index: usize,
        specs: &[AgentSpec],
        roles: &BTreeMap<&str, usize>,
        objectives: &BTreeMap<&str, usize>,
        visiting: &mut [bool],
        visited: &mut [bool],
    ) -> Result<(), KuramaError> {
        if visiting[index] {
            return Err(KuramaError::Protocol(
                "agent dependency graph contains a cycle".into(),
            ));
        }
        if visited[index] {
            return Ok(());
        }
        visiting[index] = true;
        for dependency in &specs[index].depends_on {
            let dependency = dependency_index(dependency, specs, roles, objectives)
                .ok_or_else(|| KuramaError::Protocol(format!("unknown dependency {dependency}")))?;
            visit(dependency, specs, roles, objectives, visiting, visited)?;
        }
        visiting[index] = false;
        visited[index] = true;
        Ok(())
    }

    let mut visiting = vec![false; specs.len()];
    let mut visited = vec![false; specs.len()];
    for index in 0..specs.len() {
        visit(index, specs, roles, objectives, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn infer_role(objective: &str, context: &OrchestrationContext) -> String {
    let words = objective
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    for role in context
        .role_routes
        .keys()
        .chain(context.role_escalations.keys())
    {
        let normalized = role.to_ascii_lowercase();
        if words.iter().any(|word| word == &normalized) {
            return role.clone();
        }
    }
    if contains_any(
        &words,
        &[
            "review", "reviewer", "audit", "critique", "inspect", "verify", "validate", "test",
        ],
    ) {
        "reviewer".into()
    } else if contains_any(
        &words,
        &[
            "research",
            "researcher",
            "investigate",
            "explore",
            "search",
            "discover",
            "analyze",
        ],
    ) {
        "researcher".into()
    } else if contains_any(
        &words,
        &[
            "plan",
            "planner",
            "design",
            "architect",
            "architecture",
            "spec",
        ],
    ) {
        "planner".into()
    } else {
        "implementer".into()
    }
}

fn contains_any(words: &[String], candidates: &[&str]) -> bool {
    words.iter().any(|word| candidates.contains(&word.as_str()))
}

fn canonical_scope(scope: WriteScope) -> WriteScope {
    WriteScope {
        roots: scope.roots.into_iter().map(canonical_or_original).collect(),
        files: scope.files.into_iter().map(canonical_or_original).collect(),
    }
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn scope_is_subset(child: &WriteScope, parent: &WriteScope) -> bool {
    if child.is_read_only() {
        return true;
    }
    if parent.is_read_only() {
        return false;
    }
    child.roots.iter().all(|path| scope_contains(parent, path))
        && child.files.iter().all(|path| scope_contains(parent, path))
}

fn scope_contains(scope: &WriteScope, path: &Path) -> bool {
    let path = canonical_or_original(path.to_owned());
    scope.roots.iter().any(|root| {
        let root = canonical_or_original(root.clone());
        path.starts_with(root)
    }) || scope
        .files
        .iter()
        .any(|file| canonical_or_original(file.clone()) == path)
}

pub(crate) fn scopes_overlap(left: &WriteScope, right: &WriteScope) -> bool {
    if left.is_read_only() || right.is_read_only() {
        return false;
    }
    let left_paths = left.roots.iter().chain(&left.files);
    let right_paths: Vec<_> = right.roots.iter().chain(&right.files).collect();
    left_paths.into_iter().any(|left| {
        let left = canonical_or_original(left.clone());
        right_paths.iter().any(|right| {
            let right = canonical_or_original((*right).clone());
            left.starts_with(&right) || right.starts_with(&left)
        })
    })
}

#[derive(Default)]
struct LocalIds {
    next: AtomicU64,
}

impl LocalIds {
    fn next(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next.fetch_add(1, Ordering::Relaxed))
    }
}

impl IdGenerator for LocalIds {
    fn session_id(&self) -> SessionId {
        self.next("s").into()
    }

    fn agent_id(&self) -> AgentId {
        self.next("a").into()
    }

    fn operation_id(&self) -> OperationId {
        self.next("o").into()
    }

    fn call_id(&self) -> CallId {
        self.next("c").into()
    }
}
