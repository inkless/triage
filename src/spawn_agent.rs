use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::NewAgentConfig;
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

pub fn launch(config: &NewAgentConfig, cwd: &Path) -> io::Result<String> {
    let window_name = render_window_name(config, cwd);
    tmux::new_window(&window_name, cwd, &config.command)?;
    Ok(window_name)
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
}
