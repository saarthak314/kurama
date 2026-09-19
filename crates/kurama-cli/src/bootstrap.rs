use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::args::{Args, ResumeChoice};
use kurama_adapters::{
    AppPaths, BashTool, ClaudeNativeSearch, CodexNativeSearch, ConfigRepository,
    CredentialResolver, FsSessionStore, HttpClient, JsonSearchBackend, OpenAiNativeSearch,
    ProviderFactory, ReadTool, SearchBackend, SessionSecrets, WebSearchTool, WriteTool,
};
use kurama_protocol::{
    config::{AuthRef, KuramaConfig, ProfileConfig, ProfileKind, SearchConfig},
    id::SessionId,
    model::ModelProfile,
    policy::ExecutionMode,
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    traits::{EventSink, SessionStore, Tool},
};
use kurama_sdk::Agent;

/// Shared configuration and runtime construction, without any terminal state.
pub(crate) struct Bootstrap {
    pub project: PathBuf,
    pub paths: AppPaths,
    pub session_secrets: Arc<Mutex<SessionSecrets>>,
    pub repository: ConfigRepository,
    pub store: Arc<FsSessionStore>,
    pub profiles: Vec<String>,
    pub max_input_tokens: u64,
    pub state: BootstrapState,
}

pub(crate) enum BootstrapState {
    Onboarding,
    Credential {
        profile: String,
        mode: ExecutionMode,
    },
    Connected(Box<Connection>),
}

pub(crate) struct Connection {
    pub agent: Agent,
    pub metadata: SessionMetadata,
    pub replay: Vec<EventEnvelope>,
    pub model: String,
    pub resumed_yolo: bool,
}

pub(crate) fn prepare(
    args: &Args,
    cwd: PathBuf,
    paths: AppPaths,
    session_secrets: Arc<Mutex<SessionSecrets>>,
    mode_override: Option<ExecutionMode>,
    tool_sink: Arc<dyn EventSink>,
) -> Result<Bootstrap, String> {
    let project = cwd
        .canonicalize()
        .map_err(|error| format!("canonicalize project: {error}"))?;
    let repository = ConfigRepository::open(paths.clone()).map_err(|error| error.to_string())?;
    let store = Arc::new(
        FsSessionStore::open(paths.root().to_path_buf()).map_err(|error| error.to_string())?,
    );
    let mut prepared = Bootstrap {
        project,
        paths,
        session_secrets,
        repository,
        store,
        profiles: Vec::new(),
        max_input_tokens: 0,
        state: BootstrapState::Onboarding,
    };
    let project = &prepared.project;
    let paths = &prepared.paths;
    let session_secrets = &prepared.session_secrets;
    let repository = &prepared.repository;
    let store = &prepared.store;
    let Some(config) = repository
        .read_config()
        .map_err(|error| error.to_string())?
    else {
        if args.profile.is_some() || args.resume.is_some() {
            return Err("Kurama is not configured; create ~/.kurama/config.toml first".into());
        }
        return Ok(prepared);
    };
    if config.profiles.is_empty() {
        return Ok(prepared);
    }
    let resume_id = resolve_resume(args, repository, store.as_ref(), project)?;
    let mut replay = resume_id
        .as_ref()
        .map(|session_id| store.replay(session_id))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let previous_metadata = replay.iter().find_map(|event| {
        if let SessionEvent::SessionStarted { metadata } = &event.event {
            Some(metadata.clone())
        } else {
            None
        }
    });
    if resume_id.is_some() && previous_metadata.is_none() {
        return Err("resumed session has no durable session metadata".into());
    }
    if let Some(metadata) = &previous_metadata {
        let recorded = PathBuf::from(&metadata.project_root);
        if recorded.canonicalize().ok().as_ref() != Some(project) {
            return Err(format!(
                "session {} belongs to another project",
                metadata.id
            ));
        }
        if args
            .profile
            .as_deref()
            .is_some_and(|profile| profile != metadata.profile)
        {
            return Err(format!(
                "session {} is pinned to profile {}; start a new session to switch profiles",
                metadata.id, metadata.profile
            ));
        }
    }
    let active_profile = if let Some(metadata) = &previous_metadata {
        metadata.profile.clone()
    } else {
        repository
            .resolve_profile(project, args.profile.as_deref())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "no active profile is configured".to_owned())?
    };
    let active = config
        .profiles
        .get(&active_profile)
        .ok_or_else(|| format!("unknown active profile: {active_profile}"))?;
    let mutable_state = repository.read_state().map_err(|error| error.to_string())?;
    let mode = if let Some(mode) = mode_override {
        mode
    } else if args.yolo {
        ExecutionMode::Yolo
    } else if let Some(metadata) = &previous_metadata {
        if metadata.mode == ExecutionMode::Yolo {
            mutable_state.last_mode.unwrap_or(ExecutionMode::Supervised)
        } else {
            metadata.mode
        }
    } else {
        mutable_state.last_mode.unwrap_or(config.default_mode)
    };
    let profile_names: Vec<_> = config.profiles.keys().cloned().collect();
    let active_session_missing = {
        let secrets = session_secrets
            .lock()
            .map_err(|_| "session credential store is unavailable".to_owned())?;
        matches!(active.auth, Some(AuthRef::Session)) && !secrets.contains(&active_profile)
    };
    prepared.profiles = profile_names;
    prepared.max_input_tokens = active.max_input_tokens;
    if active_session_missing {
        prepared.state = BootstrapState::Credential {
            profile: active_profile,
            mode,
        };
        return Ok(prepared);
    }

    let http = HttpClient::try_new().map_err(|error| error.to_string())?;
    let credentials = CredentialResolver;
    let provider_factory = ProviderFactory::new(http.clone(), paths.clone(), credentials);
    let mut agent = Agent::new()
        .workspace(project.clone())
        .mode(mode)
        .active_profile(active_profile.clone())
        .store(store.clone())
        .config(&config)
        .orchestrate();
    let secrets = session_secrets
        .lock()
        .map_err(|_| "session credential store is unavailable".to_owned())?;
    for (name, profile) in &config.profiles {
        if matches!(profile.auth, Some(AuthRef::Session)) && !secrets.contains(name) {
            continue;
        }
        let model_profile = ModelProfile::new(
            name.clone(),
            profile.model.clone(),
            profile.max_input_tokens,
            profile.max_output_tokens,
        );
        let backend = provider_factory
            .build(name, profile, &secrets)
            .map_err(|error| error.to_string())?;
        agent = agent.profile(model_profile, backend);
    }
    let search_backend = search_backend(
        &config,
        active_profile.as_str(),
        active,
        &secrets,
        credentials,
        http.clone(),
    )?;
    drop(secrets);
    let agent = agent
        .tools(standard_tools(http, search_backend, Some(tool_sink)))
        .build()
        .map_err(|error| error.to_string())?;

    let session_id = resume_id.unwrap_or_else(|| agent.allocate_session_id());
    let created_at_ms = previous_metadata
        .as_ref()
        .map_or_else(now_ms, |metadata| metadata.created_at_ms);
    let resumed_yolo = previous_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.mode == ExecutionMode::Yolo)
        && mode != ExecutionMode::Yolo;
    if previous_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.mode != mode)
    {
        let event = EventEnvelope::new(
            replay.last().map_or(0, |event| event.sequence + 1),
            now_ms(),
            session_id.clone(),
            None,
            SessionEvent::ModeSelected { mode },
        );
        store.append(&event).map_err(|error| error.to_string())?;
        replay.push(event);
    }
    let metadata = SessionMetadata {
        id: session_id.clone(),
        created_at_ms,
        project_root: project.display().to_string(),
        profile: active_profile.clone(),
        mode,
        redaction_best_effort: mode == ExecutionMode::Yolo,
    };

    repository
        .remember_session(
            project,
            &active_profile,
            &session_id,
            (mode_override.is_none() && mode != ExecutionMode::Yolo).then_some(mode),
        )
        .map_err(|error| error.to_string())?;

    prepared.state = BootstrapState::Connected(Box::new(Connection {
        agent,
        metadata,
        replay,
        model: active.model.clone(),
        resumed_yolo,
    }));
    Ok(prepared)
}

pub(crate) fn standard_tools(
    http: HttpClient,
    search_backend: Option<Arc<dyn SearchBackend>>,
    bash_sink: Option<Arc<dyn EventSink>>,
) -> Vec<Arc<dyn Tool>> {
    let bash: Arc<dyn Tool> = match bash_sink {
        Some(sink) => Arc::new(BashTool::with_event_sink("/bin/bash", sink)),
        None => Arc::new(BashTool::default()),
    };
    vec![
        bash,
        Arc::new(ReadTool::default()),
        Arc::new(WebSearchTool::new(http, search_backend)),
        Arc::new(WriteTool::default()),
    ]
}

fn search_backend(
    config: &KuramaConfig,
    active_profile: &str,
    active: &ProfileConfig,
    session_secrets: &SessionSecrets,
    credentials: CredentialResolver,
    http: HttpClient,
) -> Result<Option<Arc<dyn SearchBackend>>, String> {
    match config.search.as_ref() {
        Some(SearchConfig::Json { endpoint, auth }) => {
            let secret = credentials
                .resolve_optional("search", auth.as_ref(), session_secrets)
                .map_err(|error| error.to_string())?;
            Ok(Some(Arc::new(JsonSearchBackend::with_client(
                http,
                endpoint.clone(),
                secret,
            ))))
        }
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::OpenAi => {
            let auth = active
                .auth
                .as_ref()
                .ok_or_else(|| "OpenAI native search requires profile auth".to_owned())?;
            let secret = credentials
                .resolve(active_profile, auth, session_secrets)
                .map_err(|error| error.to_string())?;
            Ok(Some(Arc::new(OpenAiNativeSearch::new(
                http,
                active
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                secret,
                active.model.clone(),
            ))))
        }
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::CodexCli => {
            Ok(Some(Arc::new(CodexNativeSearch::new(
                active.command.as_deref().unwrap_or("codex"),
                active.model.clone(),
            ))))
        }
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::ClaudeCli => {
            Ok(Some(Arc::new(ClaudeNativeSearch::new(
                active.command.as_deref().unwrap_or("claude"),
                active.model.clone(),
            ))))
        }
        Some(SearchConfig::Provider) => Err(format!(
            "profile {active_profile} does not support native web search; configure [search] kind = \"json\" with a search endpoint"
        )),
        None => Ok(None),
    }
}

fn resolve_resume(
    args: &Args,
    repository: &ConfigRepository,
    store: &dyn SessionStore,
    project: &Path,
) -> Result<Option<SessionId>, String> {
    match &args.resume {
        None => Ok(None),
        Some(ResumeChoice::Id(session_id)) => Ok(Some(SessionId::from(session_id.clone()))),
        Some(ResumeChoice::Continue) => {
            let state = repository.read_state().map_err(|error| error.to_string())?;
            if let Some(session_id) = state.latest_sessions.get(project) {
                return Ok(Some(session_id.clone()));
            }
            let session = store
                .list()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|summary| {
                    PathBuf::from(&summary.project_root)
                        .canonicalize()
                        .ok()
                        .as_ref()
                        == Some(&project.to_path_buf())
                })
                .map(|summary| summary.id)
                .ok_or_else(|| "no resumable session exists for this project".to_owned())?;
            Ok(Some(session))
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
