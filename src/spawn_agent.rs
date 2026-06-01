use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, NewAgentConfig, NewAgentProvider};
use crate::models::Session;
use crate::tmux;

pub fn cwd_choices(sessions: &[Session]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut choices = Vec::new();
    for session in sessions {
        if seen.insert(session.cwd.clone()) {
            choices.push(session.cwd.clone());
        }
    }
    if choices.is_empty() {
        choices.push(default_cwd());
    }
    choices
}

pub fn default_cwd() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub struct LaunchOptions {
    pub cwd: PathBuf,
    pub config: NewAgentConfig,
    pub append_system_prompt: Option<PathBuf>,
    pub after_boot: Option<String>,
    pub detached: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub window_name: String,
    pub pane_id: String,
}

pub fn launch(config: &NewAgentConfig, cwd: &Path) -> io::Result<LaunchOutcome> {
    launch_with_options(LaunchOptions {
        cwd: cwd.to_path_buf(),
        config: config.clone(),
        append_system_prompt: None,
        after_boot: None,
        detached: false,
    })
}

pub fn launch_with_options(options: LaunchOptions) -> io::Result<LaunchOutcome> {
    let window_name = render_window_name(&options.config, &options.cwd);
    let command = build_launch_command(&options.config, options.append_system_prompt.as_deref())?;
    let pane_id = tmux::new_window(&window_name, &options.cwd, &command, options.detached)?;
    if let Some(after_boot) = options.after_boot.as_deref() {
        tmux::send_after_boot(&pane_id, after_boot, Duration::from_secs(4))?;
    }
    Ok(LaunchOutcome {
        window_name,
        pane_id,
    })
}

pub fn cli_launch(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", launch_usage(""));
        return 0;
    }
    match run_launch(args) {
        Ok(outcome) => {
            println!(
                "launched {} in {} ({})",
                outcome.provider, outcome.window_name, outcome.pane_id
            );
            0
        }
        Err(e) => {
            eprintln!("{}", e.message);
            e.code
        }
    }
}

struct LaunchCliOutcome {
    provider: String,
    window_name: String,
    pane_id: String,
}

#[derive(Debug)]
struct LaunchError {
    code: i32,
    message: String,
}

impl LaunchError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    fn launch(message: impl Into<String>) -> Self {
        Self {
            code: 4,
            message: message.into(),
        }
    }
}

#[derive(Default)]
struct LaunchArgs {
    cwd: Option<PathBuf>,
    provider: Option<NewAgentProvider>,
    window_name: Option<String>,
    command: Option<String>,
    append_system_prompt: Option<PathBuf>,
    after_boot: Option<String>,
}

fn run_launch(args: &[String]) -> Result<LaunchCliOutcome, LaunchError> {
    let args = parse_launch_args(args)?;
    let cwd = args
        .cwd
        .ok_or_else(|| LaunchError::usage(launch_usage("missing --cwd")))?;
    if !cwd.is_dir() {
        return Err(LaunchError::usage(format!(
            "--cwd is not a directory: {}",
            cwd.display()
        )));
    }

    let mut config = Config::load().new_agent;
    let explicit_provider = args.provider;
    if let Some(provider) = explicit_provider {
        config.provider = provider;
        config.command = provider.default_command().to_string();
    }
    if let Some(command) = args.command {
        if explicit_provider.is_none()
            && let Some(provider) = infer_provider_from_command(&command)
        {
            config.provider = provider;
        }
        config.command = command;
    }
    if let Some(window_name) = args.window_name {
        config.window_name = window_name;
    }
    if let Some(text) = &args.after_boot {
        validate_after_boot(text)?;
    }

    let provider = config.provider.name().to_string();
    let outcome = launch_with_options(LaunchOptions {
        cwd,
        config,
        append_system_prompt: args.append_system_prompt,
        after_boot: args.after_boot,
        detached: true,
    })
    .map_err(|e| LaunchError::launch(format!("launch failed: {e}")))?;

    Ok(LaunchCliOutcome {
        provider,
        window_name: outcome.window_name,
        pane_id: outcome.pane_id,
    })
}

pub fn render_window_name(config: &NewAgentConfig, cwd: &Path) -> String {
    let cwd_basename = cwd_basename(cwd);
    let cwd_display = sanitize_window_name(&cwd.display().to_string());
    let rendered = config
        .window_name
        .replace("{provider}", config.provider.name())
        .replace("{cwd_basename}", &cwd_basename)
        .replace("{cwd}", &cwd_display);
    let rendered = sanitize_window_name(&rendered);
    if rendered.is_empty() {
        format!("agent-{}-{cwd_basename}", config.provider.name())
    } else {
        truncate_chars(&rendered, 80)
    }
}

fn cwd_basename(cwd: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && cwd == Path::new(&home)
    {
        return "home".to_string();
    }
    cwd.file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "root".to_string())
}

fn sanitize_window_name(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

fn parse_launch_args(args: &[String]) -> Result<LaunchArgs, LaunchError> {
    let mut out = LaunchArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cwd" => {
                i += 1;
                out.cwd = Some(PathBuf::from(args.get(i).ok_or_else(|| {
                    LaunchError::usage(launch_usage("missing --cwd value"))
                })?));
            }
            "--provider" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| LaunchError::usage(launch_usage("missing --provider value")))?;
                out.provider = Some(NewAgentProvider::parse(value).ok_or_else(|| {
                    LaunchError::usage(launch_usage(format!(
                        "unknown provider {value:?}; expected claude or codex"
                    )))
                })?);
            }
            "--window-name" => {
                i += 1;
                out.window_name = Some(
                    args.get(i)
                        .ok_or_else(|| {
                            LaunchError::usage(launch_usage("missing --window-name value"))
                        })?
                        .trim()
                        .to_string(),
                );
            }
            "--command" => {
                i += 1;
                out.command = Some(
                    args.get(i)
                        .ok_or_else(|| LaunchError::usage(launch_usage("missing --command value")))?
                        .trim()
                        .to_string(),
                );
            }
            "--append-system-prompt" => {
                i += 1;
                out.append_system_prompt = Some(PathBuf::from(args.get(i).ok_or_else(|| {
                    LaunchError::usage(launch_usage("missing --append-system-prompt value"))
                })?));
            }
            "--after-boot" => {
                i += 1;
                out.after_boot = Some(
                    args.get(i)
                        .ok_or_else(|| {
                            LaunchError::usage(launch_usage("missing --after-boot value"))
                        })?
                        .to_string(),
                );
            }
            other => {
                return Err(LaunchError::usage(launch_usage(format!(
                    "unknown arg {other:?}"
                ))));
            }
        }
        i += 1;
    }
    Ok(out)
}

fn build_launch_command(
    config: &NewAgentConfig,
    append_system_prompt: Option<&Path>,
) -> io::Result<String> {
    let mut command = config.command.clone();
    if config.provider == NewAgentProvider::Claude
        && let Some(path) = append_system_prompt
    {
        validate_prompt_file(path)?;
        command.push_str(" --append-system-prompt \"$(cat ");
        command.push_str(&tmux::shell_quote(&path.display().to_string()));
        command.push_str(")\"");
    }
    Ok(command)
}

fn validate_prompt_file(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "prompt path is not a file: {}",
            path.display()
        )));
    }
    let _ = fs::File::open(path)?;
    Ok(())
}

fn validate_after_boot(text: &str) -> Result<(), LaunchError> {
    if text.trim().is_empty() {
        return Err(LaunchError::usage("--after-boot text is empty"));
    }
    for c in text.chars() {
        if c == '\n' || c == '\t' || !c.is_control() {
            continue;
        }
        return Err(LaunchError::usage(format!(
            "--after-boot contains unsupported control character U+{:04X}",
            c as u32
        )));
    }
    Ok(())
}

fn infer_provider_from_command(command: &str) -> Option<NewAgentProvider> {
    let first = command.split_whitespace().next()?;
    let binary = PathBuf::from(first);
    let name = binary.file_name().and_then(|n| n.to_str()).unwrap_or(first);
    NewAgentProvider::parse(name)
}

fn launch_usage(prefix: impl Into<String>) -> String {
    let prefix = prefix.into();
    let usage = "usage: triage launch --cwd PATH [--provider claude|codex] [--window-name NAME] [--command CMD] [--append-system-prompt FILE] [--after-boot TEXT]";
    if prefix.is_empty() {
        usage.to_string()
    } else {
        format!("{prefix}\n{usage}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NewAgentProvider;
    use crate::models::Provider;

    fn session(cwd: &str) -> Session {
        Session::new(
            Provider::Claude,
            1,
            "sid".to_string(),
            PathBuf::from(cwd),
            None,
            "idle".to_string(),
            0,
            0,
            None,
        )
    }

    #[test]
    fn cwd_choices_dedupe_in_session_order() {
        let choices = cwd_choices(&[
            session("/tmp/one"),
            session("/tmp/two"),
            session("/tmp/one"),
        ]);

        assert_eq!(
            choices,
            vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
        );
    }

    #[test]
    fn cwd_choices_falls_back_to_home_when_empty() {
        let choices = cwd_choices(&[]);

        assert_eq!(choices.len(), 1);
        assert!(!choices[0].as_os_str().is_empty());
    }

    #[test]
    fn window_name_renders_configured_placeholders() {
        let cfg = NewAgentConfig {
            provider: NewAgentProvider::Codex,
            command: "codex".to_string(),
            window_name: "{provider}-{cwd_basename}".to_string(),
        };

        assert_eq!(
            render_window_name(&cfg, Path::new("/Users/example/workspace/triage")),
            "codex-triage"
        );
    }

    #[test]
    fn launch_args_parse_all_options() {
        let args = vec![
            "--cwd".to_string(),
            "/tmp/project".to_string(),
            "--provider".to_string(),
            "codex".to_string(),
            "--window-name".to_string(),
            "agent-test".to_string(),
            "--command".to_string(),
            "codex --model gpt-5".to_string(),
            "--append-system-prompt".to_string(),
            "/tmp/prompt.md".to_string(),
            "--after-boot".to_string(),
            "hello".to_string(),
        ];

        let parsed = parse_launch_args(&args).unwrap();

        assert_eq!(parsed.cwd, Some(PathBuf::from("/tmp/project")));
        assert_eq!(parsed.provider, Some(NewAgentProvider::Codex));
        assert_eq!(parsed.window_name, Some("agent-test".to_string()));
        assert_eq!(parsed.command, Some("codex --model gpt-5".to_string()));
        assert_eq!(
            parsed.append_system_prompt,
            Some(PathBuf::from("/tmp/prompt.md"))
        );
        assert_eq!(parsed.after_boot, Some("hello".to_string()));
    }

    #[test]
    fn provider_can_be_inferred_from_command_path() {
        assert_eq!(
            infer_provider_from_command("/opt/homebrew/bin/codex --model gpt-5"),
            Some(NewAgentProvider::Codex)
        );
        assert_eq!(
            infer_provider_from_command("claude --model sonnet"),
            Some(NewAgentProvider::Claude)
        );
    }

    #[test]
    fn launch_command_appends_claude_prompt_file() {
        let prompt_path = std::env::temp_dir().join(format!(
            "triage-launch-prompt-{}-{}.md",
            std::process::id(),
            "claude"
        ));
        fs::write(&prompt_path, "system prompt").unwrap();
        let cfg = NewAgentConfig {
            provider: NewAgentProvider::Claude,
            command: "claude --model sonnet".to_string(),
            window_name: "agent".to_string(),
        };

        let command = build_launch_command(&cfg, Some(&prompt_path)).unwrap();

        assert!(command.starts_with("claude --model sonnet --append-system-prompt "));
        assert!(command.contains("$(cat "));
        assert!(command.contains(&prompt_path.display().to_string()));
        let _ = fs::remove_file(prompt_path);
    }

    #[test]
    fn launch_command_ignores_prompt_file_for_codex() {
        let cfg = NewAgentConfig {
            provider: NewAgentProvider::Codex,
            command: "codex --model gpt-5".to_string(),
            window_name: "agent".to_string(),
        };

        assert_eq!(
            build_launch_command(&cfg, Some(Path::new("/definitely/missing/prompt.md"))).unwrap(),
            "codex --model gpt-5"
        );
    }
}
