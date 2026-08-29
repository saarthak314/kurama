use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use kurama_protocol::{
    KuramaError,
    config::{
        KuramaConfig, MutableState, OrchestrationConfig, ProfileConfig, ProfileKind, RoleConfig,
        SearchConfig,
    },
    id::SessionId,
    policy::{AutoBoundaries, ExecutionMode},
};

use crate::credentials::{format_auth_ref, parse_auth_ref};

const CONFIG_VERSION: u32 = 1;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    sessions: PathBuf,
    blobs: PathBuf,
    cache: PathBuf,
}

impl AppPaths {
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            config: root.join("config.toml"),
            state: root.join("state.json"),
            sessions: root.join("sessions"),
            blobs: root.join("blobs"),
            cache: root.join("cache"),
            root,
        }
    }

    pub fn discover() -> Result<Self, KuramaError> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| KuramaError::Configuration("home directory is unavailable".into()))?;
        Ok(Self::from_root(PathBuf::from(home).join(".kurama")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn sessions(&self) -> &Path {
        &self.sessions
    }

    pub fn blobs(&self) -> &Path {
        &self.blobs
    }

    pub fn cache(&self) -> &Path {
        &self.cache
    }
}

#[derive(Debug, Clone)]
pub struct ConfigRepository {
    paths: AppPaths,
}

impl ConfigRepository {
    pub fn open(paths: AppPaths) -> Result<Self, KuramaError> {
        ensure_directory(paths.root())?;
        for directory in [paths.sessions(), paths.blobs(), paths.cache()] {
            ensure_directory(directory)?;
        }
        ensure_file(paths.config(), b"")?;
        let state = serde_json::to_vec(&MutableState::default())
            .map_err(|error| configuration_error("serialize initial state", error))?;
        ensure_file(paths.state(), &state)?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn read_config(&self) -> Result<Option<KuramaConfig>, KuramaError> {
        let source = fs::read_to_string(self.paths.config())?;
        if source.trim().is_empty() {
            return Ok(None);
        }
        let document: ConfigDocument = basic_toml::from_str(&source)
            .map_err(|error| configuration_error("parse config.toml", error))?;
        let config = document.into_config()?;
        validate_config(&config)?;
        Ok(Some(config))
    }

    pub fn write_config(&self, config: &KuramaConfig) -> Result<(), KuramaError> {
        validate_config(config)?;
        let document = ConfigDocument::from_config(config);
        let encoded = basic_toml::to_string(&document)
            .map_err(|error| configuration_error("serialize config.toml", error))?;
        atomic_write(self.paths.config(), encoded.as_bytes())
    }

    pub fn read_state(&self) -> Result<MutableState, KuramaError> {
        let bytes = fs::read(self.paths.state())?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(MutableState::default());
        }
        let state: MutableState = serde_json::from_slice(&bytes)
            .map_err(|error| configuration_error("parse state.json", error))?;
        validate_state(&state)?;
        Ok(state)
    }

    pub fn write_state(&self, state: &MutableState) -> Result<(), KuramaError> {
        validate_state(state)?;
        let bytes = serde_json::to_vec(state)
            .map_err(|error| configuration_error("serialize state.json", error))?;
        atomic_write(self.paths.state(), &bytes)
    }

    pub fn resolve_profile(
        &self,
        project: &Path,
        cli_override: Option<&str>,
    ) -> Result<Option<String>, KuramaError> {
        let config = self.read_config()?;
        if let Some(profile) = cli_override {
            validate_profile_reference(config.as_ref(), profile)?;
            return Ok(Some(profile.to_owned()));
        }

        let project = canonical_project(project)?;
        let state = self.read_state()?;
        if let Some(profile) = state.project_profiles.get(&project) {
            validate_profile_reference(config.as_ref(), profile)?;
            return Ok(Some(profile.clone()));
        }
        Ok(config.and_then(|config| config.default_profile))
    }

    pub fn remember_project_profile(
        &self,
        project: &Path,
        profile: &str,
    ) -> Result<(), KuramaError> {
        let config = self.read_config()?;
        validate_profile_reference(config.as_ref(), profile)?;
        let mut state = self.read_state()?;
        state
            .project_profiles
            .insert(canonical_project(project)?, profile.to_owned());
        self.write_state(&state)
    }

    pub fn remember_latest_session(
        &self,
        project: &Path,
        session_id: &SessionId,
    ) -> Result<(), KuramaError> {
        let mut state = self.read_state()?;
        state
            .latest_sessions
            .insert(canonical_project(project)?, session_id.clone());
        self.write_state(&state)
    }

    pub fn remember_mode(&self, mode: ExecutionMode) -> Result<(), KuramaError> {
        if mode == ExecutionMode::Yolo {
            return Err(KuramaError::Configuration(
                "YOLO is launch-only and cannot be persisted".into(),
            ));
        }
        let mut state = self.read_state()?;
        state.last_mode = Some(mode);
        self.write_state(&state)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u32,
    default_profile: Option<String>,
    #[serde(default)]
    default_mode: ExecutionMode,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileDocument>,
    #[serde(default)]
    roles: BTreeMap<String, RoleConfig>,
    #[serde(default)]
    orchestration: OrchestrationConfig,
    #[serde(default)]
    policy: PolicyDocument,
    search: Option<SearchDocument>,
}

impl ConfigDocument {
    fn from_config(config: &KuramaConfig) -> Self {
        Self {
            version: config.version,
            default_profile: config.default_profile.clone(),
            default_mode: config.default_mode,
            profiles: config
                .profiles
                .iter()
                .map(|(name, profile)| (name.clone(), ProfileDocument::from_config(profile)))
                .collect(),
            roles: config.roles.clone(),
            orchestration: config.orchestration.clone(),
            policy: PolicyDocument {
                auto: config.auto.clone(),
            },
            search: config.search.as_ref().map(SearchDocument::from_config),
        }
    }

    fn into_config(self) -> Result<KuramaConfig, KuramaError> {
        Ok(KuramaConfig {
            version: self.version,
            default_profile: self.default_profile,
            default_mode: self.default_mode,
            profiles: self
                .profiles
                .into_iter()
                .map(|(name, profile)| profile.into_config().map(|profile| (name, profile)))
                .collect::<Result<_, _>>()?,
            roles: self.roles,
            orchestration: self.orchestration,
            auto: self.policy.auto,
            search: self.search.map(SearchDocument::into_config).transpose()?,
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDocument {
    kind: ProfileKindDocument,
    model: String,
    endpoint: Option<String>,
    auth: Option<String>,
    command: Option<String>,
    #[serde(default)]
    max_input_tokens: u64,
    #[serde(default)]
    max_output_tokens: u64,
    #[serde(default)]
    escalation_profiles: Vec<String>,
}

impl ProfileDocument {
    fn from_config(config: &ProfileConfig) -> Self {
        Self {
            kind: ProfileKindDocument::from(config.kind.clone()),
            model: config.model.clone(),
            endpoint: config.endpoint.clone(),
            auth: config.auth.as_ref().map(format_auth_ref),
            command: config.command.clone(),
            max_input_tokens: config.max_input_tokens,
            max_output_tokens: config.max_output_tokens,
            escalation_profiles: config.escalation_profiles.clone(),
        }
    }

    fn into_config(self) -> Result<ProfileConfig, KuramaError> {
        Ok(ProfileConfig {
            kind: self.kind.into(),
            model: self.model,
            endpoint: self.endpoint,
            auth: self.auth.as_deref().map(parse_auth_ref).transpose()?,
            command: self.command,
            max_input_tokens: self.max_input_tokens,
            max_output_tokens: self.max_output_tokens,
            escalation_profiles: self.escalation_profiles,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProfileKindDocument {
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
    OpenAiCompatible,
    CodexCli,
    ClaudeCli,
}

impl From<ProfileKind> for ProfileKindDocument {
    fn from(value: ProfileKind) -> Self {
        match value {
            ProfileKind::OpenAi => Self::OpenAi,
            ProfileKind::Anthropic => Self::Anthropic,
            ProfileKind::OpenAiCompatible => Self::OpenAiCompatible,
            ProfileKind::CodexCli => Self::CodexCli,
            ProfileKind::ClaudeCli => Self::ClaudeCli,
        }
    }
}

impl From<ProfileKindDocument> for ProfileKind {
    fn from(value: ProfileKindDocument) -> Self {
        match value {
            ProfileKindDocument::OpenAi => Self::OpenAi,
            ProfileKindDocument::Anthropic => Self::Anthropic,
            ProfileKindDocument::OpenAiCompatible => Self::OpenAiCompatible,
            ProfileKindDocument::CodexCli => Self::CodexCli,
            ProfileKindDocument::ClaudeCli => Self::ClaudeCli,
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    #[serde(default)]
    auto: AutoBoundaries,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchDocument {
    kind: SearchKindDocument,
    endpoint: Option<String>,
    auth: Option<String>,
}

impl SearchDocument {
    fn from_config(config: &SearchConfig) -> Self {
        match config {
            SearchConfig::Provider => Self {
                kind: SearchKindDocument::Provider,
                endpoint: None,
                auth: None,
            },
            SearchConfig::Json { endpoint, auth } => Self {
                kind: SearchKindDocument::Json,
                endpoint: Some(endpoint.clone()),
                auth: auth.as_ref().map(format_auth_ref),
            },
        }
    }

    fn into_config(self) -> Result<SearchConfig, KuramaError> {
        match self.kind {
            SearchKindDocument::Provider => {
                if self.endpoint.is_some() || self.auth.is_some() {
                    return Err(KuramaError::Configuration(
                        "provider search does not accept endpoint or auth".into(),
                    ));
                }
                Ok(SearchConfig::Provider)
            }
            SearchKindDocument::Json => {
                let endpoint = self
                    .endpoint
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        KuramaError::Configuration("JSON search endpoint is required".into())
                    })?;
                Ok(SearchConfig::Json {
                    endpoint,
                    auth: self.auth.as_deref().map(parse_auth_ref).transpose()?,
                })
            }
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchKindDocument {
    Provider,
    Json,
}

fn validate_config(config: &KuramaConfig) -> Result<(), KuramaError> {
    if config.version != CONFIG_VERSION {
        return Err(KuramaError::Configuration(format!(
            "unsupported config version {}; expected {CONFIG_VERSION}",
            config.version
        )));
    }
    if config.default_mode == ExecutionMode::Yolo {
        return Err(KuramaError::Configuration(
            "YOLO is launch-only and cannot be persisted".into(),
        ));
    }
    if !(1..=8).contains(&config.orchestration.max_concurrency) {
        return Err(KuramaError::Configuration(
            "orchestration.max_concurrency must be between 1 and 8".into(),
        ));
    }
    if let Some(default_profile) = &config.default_profile {
        require_known_profile(config, default_profile, "default_profile")?;
    }
    for (name, profile) in &config.profiles {
        if name.trim().is_empty() || profile.model.trim().is_empty() {
            return Err(KuramaError::Configuration(
                "profile names and models cannot be empty".into(),
            ));
        }
        if profile
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.trim().is_empty())
            || profile
                .command
                .as_ref()
                .is_some_and(|command| command.trim().is_empty())
        {
            return Err(KuramaError::Configuration(format!(
                "profile {name} contains an empty endpoint or command"
            )));
        }
        if profile.kind == ProfileKind::OpenAiCompatible && profile.endpoint.is_none() {
            return Err(KuramaError::Configuration(format!(
                "profile {name} requires an endpoint"
            )));
        }
        if matches!(profile.kind, ProfileKind::CodexCli | ProfileKind::ClaudeCli)
            && profile.command.is_none()
        {
            return Err(KuramaError::Configuration(format!(
                "profile {name} requires a command"
            )));
        }
        if let Some(auth) = &profile.auth {
            validate_auth_reference(auth)?;
        }
        for escalation in &profile.escalation_profiles {
            require_known_profile(config, escalation, "profile escalation")?;
        }
    }
    for (role, route) in &config.roles {
        if role.trim().is_empty() {
            return Err(KuramaError::Configuration(
                "role name cannot be empty".into(),
            ));
        }
        require_known_profile(config, &route.profile, "role profile")?;
        for escalation in &route.escalation_profiles {
            require_known_profile(config, escalation, "role escalation")?;
        }
    }
    if let Some(SearchConfig::Json { endpoint, auth }) = &config.search {
        if endpoint.trim().is_empty() {
            return Err(KuramaError::Configuration(
                "JSON search endpoint is required".into(),
            ));
        }
        if let Some(auth) = auth {
            validate_auth_reference(auth)?;
        }
    }
    Ok(())
}

fn validate_auth_reference(auth: &kurama_protocol::config::AuthRef) -> Result<(), KuramaError> {
    let encoded = format_auth_ref(auth);
    if parse_auth_ref(&encoded)? == *auth {
        Ok(())
    } else {
        Err(KuramaError::Configuration(
            "auth reference cannot be represented safely".into(),
        ))
    }
}

fn require_known_profile(
    config: &KuramaConfig,
    profile: &str,
    field: &str,
) -> Result<(), KuramaError> {
    if config.profiles.contains_key(profile) {
        Ok(())
    } else {
        Err(KuramaError::Configuration(format!(
            "{field} references unknown profile {profile}"
        )))
    }
}

fn validate_profile_reference(
    config: Option<&KuramaConfig>,
    profile: &str,
) -> Result<(), KuramaError> {
    if profile.trim().is_empty() {
        return Err(KuramaError::Configuration(
            "profile reference cannot be empty".into(),
        ));
    }
    let config = config.ok_or_else(|| {
        KuramaError::Configuration("profile reference requires a configured profile".into())
    })?;
    require_known_profile(config, profile, "profile")
}

fn validate_state(state: &MutableState) -> Result<(), KuramaError> {
    if state.last_mode == Some(ExecutionMode::Yolo) {
        return Err(KuramaError::Configuration(
            "YOLO is launch-only and cannot be persisted".into(),
        ));
    }
    Ok(())
}

fn canonical_project(project: &Path) -> Result<PathBuf, KuramaError> {
    fs::canonicalize(project).map_err(|error| {
        KuramaError::Configuration(format!(
            "cannot canonicalize project {}: {error}",
            project.display()
        ))
    })
}

fn ensure_directory(path: &Path) -> Result<(), KuramaError> {
    if path.exists() {
        reject_symlink(path)?;
        if !path.is_dir() {
            return Err(KuramaError::Configuration(format!(
                "{} is not a directory",
                path.display()
            )));
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;

            let mut builder = fs::DirBuilder::new();
            builder.mode(DIRECTORY_MODE);
            builder.create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir(path)?;
    }
    set_directory_permissions(path)?;
    Ok(())
}

fn ensure_file(path: &Path, initial: &[u8]) -> Result<(), KuramaError> {
    if path.exists() {
        reject_symlink(path)?;
        if !path.is_file() {
            return Err(KuramaError::Configuration(format!(
                "{} is not a file",
                path.display()
            )));
        }
        set_file_permissions(path)?;
        return Ok(());
    }
    create_file(path, initial, true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), KuramaError> {
    reject_symlink(path)?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| KuramaError::Configuration(format!("system clock error: {error}")))?
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
    create_file(&temporary, bytes, true)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    set_file_permissions(path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn create_file(path: &Path, bytes: &[u8], create_new: bool) -> Result<(), KuramaError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(create_new)
        .truncate(!create_new);
    #[cfg(unix)]
    options.mode(FILE_MODE);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_file_permissions(path)?;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), KuramaError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(KuramaError::Configuration(
            format!("refusing symbolic link {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn set_directory_permissions(path: &Path) -> Result<(), KuramaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))?;
    }
    Ok(())
}

fn set_file_permissions(path: &Path) -> Result<(), KuramaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(())
}

fn configuration_error(context: &str, error: impl std::fmt::Display) -> KuramaError {
    KuramaError::Configuration(format!("{context}: {error}"))
}
