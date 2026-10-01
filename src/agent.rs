use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Claude,
    Codex,
    Pi,
}

impl AgentKind {
    const fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    pub fn detect() -> Option<Self> {
        [Self::Claude, Self::Codex, Self::Pi]
            .into_iter()
            .find(|a| on_path(a.binary()))
    }
}

pub struct Agent {
    pub kind: AgentKind,
    pub model: Option<String>,
}

impl Agent {
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{} ({m})", self.kind.binary()),
            None => self.kind.binary().to_owned(),
        }
    }

    /// Runs one non-interactive, tool-less completion through the user's own agent CLI.
    pub fn complete(&self, system: &str, prompt: &str) -> Result<String> {
        let mut cmd = Command::new(self.kind.binary());
        let stdin_text = match self.kind {
            AgentKind::Claude => {
                cmd.args(["-p", "--system-prompt", system]);
                if let Some(m) = &self.model {
                    cmd.args(["--model", m]);
                }
                prompt.to_owned()
            }
            AgentKind::Codex => {
                cmd.args(["exec", "--skip-git-repo-check", "--sandbox", "read-only"]);
                if let Some(m) = &self.model {
                    cmd.args(["-m", m]);
                }
                cmd.arg("-");
                format!("{system}\n\n{prompt}")
            }
            AgentKind::Pi => {
                cmd.args([
                    "-p",
                    "--no-session",
                    "--no-tools",
                    "--no-context-files",
                    "--no-skills",
                    "--no-prompt-templates",
                    "--system-prompt",
                    system,
                ]);
                if let Some(m) = &self.model {
                    cmd.args(["--model", m]);
                }
                prompt.to_owned()
            }
        };

        let mut child = cmd
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start `{}`", self.kind.binary()))?;
        child
            .stdin
            .take()
            .context("agent stdin unavailable")?
            .write_all(stdin_text.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "`{}` exited with {}: {}",
                self.kind.binary(),
                out.status,
                err.trim().chars().take(400).collect::<String>()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        ["", ".exe", ".cmd"]
            .iter()
            .any(|ext| dir.join(format!("{bin}{ext}")).is_file())
    })
}
