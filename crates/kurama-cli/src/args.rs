#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeChoice {
    Id(String),
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    pub profile: Option<String>,
    pub resume: Option<ResumeChoice>,
    pub yolo: bool,
    pub help: bool,
    pub version: bool,
    pub internal_print_prompt_bundle: bool,
}

pub const HELP: &str = "Kurama — minimal coding agent\n\nUsage: kurama [OPTIONS]\n\nOptions:\n  --profile <NAME>  Use a configured profile\n  --resume <ID>     Resume a session\n  --continue        Resume this project's latest session\n  --yolo            Disable approvals and boundaries for this launch\n  -h, --help        Print help\n  -V, --version     Print version\n";

pub fn parse_from<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    use lexopt::prelude::*;

    let mut parser = lexopt::Parser::from_args(args);
    let mut parsed = Args::default();
    while let Some(argument) = parser.next().map_err(|error| error.to_string())? {
        match argument {
            Long("profile") => {
                parsed.profile = Some(
                    parser
                        .value()
                        .map_err(|error| error.to_string())?
                        .string()
                        .map_err(|error| error.to_string())?,
                );
            }
            Long("resume") => {
                parsed.resume = Some(ResumeChoice::Id(
                    parser
                        .value()
                        .map_err(|error| error.to_string())?
                        .string()
                        .map_err(|error| error.to_string())?,
                ));
            }
            Long("continue") => parsed.resume = Some(ResumeChoice::Continue),
            Long("yolo") => parsed.yolo = true,
            Short('h') | Long("help") => parsed.help = true,
            Short('V') | Long("version") => parsed.version = true,
            Long("internal-print-prompt-bundle") => parsed.internal_print_prompt_bundle = true,
            value => return Err(format!("unexpected argument: {value:?}")),
        }
    }
    Ok(parsed)
}
