#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kurama_adapters::{
    AnthropicBackend, BashTool, ClaudeBridge, CodexBridge, FsSessionStore, HttpClient,
    OpenAiBackend, OpenAiCompatBackend, ReadTool, WebSearchTool, WriteTool,
};
use kurama_protocol::{
    model::ModelProfile,
    policy::ExecutionMode,
    traits::{ModelBackend, Tool},
};

const FRONTIER_INPUT_TOKENS: u64 = 200_000;
const FRONTIER_OUTPUT_TOKENS: u64 = 12_000;

pub mod prelude {
    pub use crate::Kurama;
    pub use kurama_sdk::{Agent, Event, Handle, KuramaError, Turn, TurnOutcome};
}

pub use kurama_sdk::{
    Agent, AgentBuilder, AgentSetup, Event, Events, Handle, KuramaError, Turn, TurnOutcome,
};

enum CliBackend {
    Codex,
    Claude,
}

pub struct Kurama {
    setup: AgentSetup,
    no_tools: bool,
    extra_tools: Vec<Arc<dyn Tool>>,
    persist_root: Option<PathBuf>,
    ephemeral: bool,
    cli: Option<CliBackend>,
}

impl Kurama {
    pub fn openai(api_key: impl Into<String>) -> Result<Self, KuramaError> {
        let backend = OpenAiBackend::from_endpoint(
            HttpClient::try_new()?,
            "https://api.openai.com/v1",
            api_key,
        )?;
        Ok(Self::provider("openai", "gpt-5.6", Arc::new(backend)))
    }

    pub fn anthropic(api_key: impl Into<String>) -> Result<Self, KuramaError> {
        let backend = AnthropicBackend::from_endpoint(
            HttpClient::try_new()?,
            "https://api.anthropic.com/v1",
            api_key,
        )?;
        Ok(Self::provider(
            "anthropic",
            "claude-sonnet-4-6",
            Arc::new(backend),
        ))
    }

    pub fn openai_compatible(
        endpoint: impl AsRef<str>,
        api_key: Option<String>,
    ) -> Result<Self, KuramaError> {
        let backend =
            OpenAiCompatBackend::from_endpoint(HttpClient::try_new()?, endpoint.as_ref(), api_key)?;
        Ok(Self::provider(
            "openai-compatible",
            "default",
            Arc::new(backend),
        ))
    }

    pub fn codex_cli() -> Self {
        Self::cli(
            CliBackend::Codex,
            Agent::new()
                .orchestrate()
                .model("gpt-5.6")
                .limits(FRONTIER_INPUT_TOKENS, FRONTIER_OUTPUT_TOKENS),
        )
    }

    pub fn claude_cli() -> Self {
        Self::cli(
            CliBackend::Claude,
            Agent::new()
                .orchestrate()
                .model("sonnet")
                .limits(FRONTIER_INPUT_TOKENS, FRONTIER_OUTPUT_TOKENS),
        )
    }

    pub fn from_backend(backend: impl ModelBackend + 'static) -> Self {
        Self {
            setup: Agent::new().backend(backend).orchestrate(),
            no_tools: false,
            extra_tools: Vec::new(),
            persist_root: None,
            ephemeral: false,
            cli: None,
        }
    }

    fn provider(name: &str, model: &str, backend: Arc<dyn ModelBackend>) -> Self {
        Self {
            setup: Agent::new()
                .profile(
                    ModelProfile::new(name, model, FRONTIER_INPUT_TOKENS, FRONTIER_OUTPUT_TOKENS),
                    backend,
                )
                .orchestrate(),
            no_tools: false,
            extra_tools: Vec::new(),
            persist_root: None,
            ephemeral: false,
            cli: None,
        }
    }

    fn cli(cli: CliBackend, setup: AgentSetup) -> Self {
        Self {
            setup,
            no_tools: false,
            extra_tools: Vec::new(),
            persist_root: None,
            ephemeral: false,
            cli: Some(cli),
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.setup = self.setup.model(model);
        self
    }

    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.setup = self.setup.workspace(workspace);
        self
    }

    pub fn yolo(mut self) -> Self {
        self.setup = self.setup.mode(ExecutionMode::Yolo);
        self
    }

    pub fn auto(mut self) -> Self {
        self.setup = self.setup.mode(ExecutionMode::Auto);
        self
    }

    pub fn allow_writes(mut self, roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        self.setup = self.setup.allow_writes(roots);
        self
    }

    pub fn allow_commands(mut self, commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.setup = self.setup.allow_commands(commands);
        self
    }

    pub fn allow_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.setup = self.setup.allow_hosts(hosts);
        self
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.extra_tools.push(Arc::new(tool));
        self
    }

    pub fn no_tools(mut self) -> Self {
        self.no_tools = true;
        self
    }

    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    pub fn persist(mut self, root: impl Into<PathBuf>) -> Self {
        self.persist_root = Some(root.into());
        self.ephemeral = false;
        self
    }

    pub fn build(self) -> Result<Agent, KuramaError> {
        let mut setup = self.setup;
        if let Some(cli) = self.cli {
            let root = match &self.persist_root {
                Some(root) => root.clone(),
                None => default_root()?,
            };
            let cache = root.join("cache/bridge");
            setup = match cli {
                CliBackend::Codex => setup.backend_as(
                    "codex",
                    Arc::new(CodexBridge::new(
                        cache.join("sessions/codex"),
                        cache.join("control-v1.json"),
                    )),
                ),
                CliBackend::Claude => setup.backend_as(
                    "claude",
                    Arc::new(ClaudeBridge::new(cache.join("control-v1.json"))),
                ),
            };
        }
        if !self.no_tools {
            let http = HttpClient::try_new()?;
            setup = setup.tools([
                Arc::new(BashTool::default()) as Arc<dyn Tool>,
                Arc::new(ReadTool::default()),
                Arc::new(WebSearchTool::new(http, None)),
                Arc::new(WriteTool::default()),
            ]);
        }
        for tool in self.extra_tools {
            setup = setup.tool_arc(tool);
        }
        if !self.ephemeral {
            let root = match self.persist_root {
                Some(root) => root,
                None => default_root()?,
            };
            setup = setup.store(Arc::new(FsSessionStore::open(root)?));
        }
        setup.build()
    }

    pub async fn prompt(self, text: impl Into<String>) -> Result<TurnOutcome, KuramaError> {
        let mut agent = self.build()?;
        agent.prompt(text).await
    }
}

fn default_root() -> Result<PathBuf, KuramaError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| KuramaError::Configuration("home directory is unavailable".into()))?;
    Ok(PathBuf::from(home).join(".kurama"))
}
