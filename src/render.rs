use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::ValueEnum;

use crate::distill::Rule;
use crate::github::Repo;

const START: &str = "<!-- difflore:start -->";
const END: &str = "<!-- difflore:end -->";
const HOME: &str = "https://github.com/difflore/difflore-cli";

pub struct Summary<'a> {
    pub repo: &'a Repo,
    pub comments: usize,
    pub prs: usize,
}

struct Style(bool);

impl Style {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn accent(&self, s: &str) -> String {
        self.wrap("36", s)
    }
}

pub fn terminal(summary: &Summary<'_>, rules: &[Rule]) -> String {
    let st = Style(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    let mut o = String::new();
    let _ = writeln!(
        o,
        "\n{}  {}",
        st.bold("difflore"),
        st.accent(&summary.repo.to_string())
    );
    let _ = writeln!(
        o,
        "{}",
        st.dim(&format!(
            "{} maintainer review comments across {} pull requests",
            summary.comments, summary.prs
        ))
    );
    if rules.is_empty() {
        let _ = writeln!(
            o,
            "\nNo rule came up in enough pull requests. Try a larger --limit."
        );
        return o;
    }
    let _ = writeln!(
        o,
        "\n{}\n",
        st.bold(&format!("{} unwritten rules", rules.len()))
    );
    for (i, r) in rules.iter().enumerate() {
        let _ = writeln!(
            o,
            "{:>3}  {}",
            st.bold(&(i + 1).to_string()),
            st.bold(&r.rule)
        );
        if !r.why.is_empty() {
            let _ = writeln!(o, "     {}", r.why);
        }
        let mut meta = format!("seen in {} PRs", r.prs);
        if !r.paths.is_empty() {
            let _ = write!(meta, " · {}", r.paths);
        }
        let _ = writeln!(o, "     {}", st.dim(&meta));
        if let Some(e) = r.evidence.first() {
            let _ = writeln!(o, "     {}", st.dim(&format!("↳ {}", e.url)));
        }
        o.push('\n');
    }
    o
}

pub fn markdown_block(summary: &Summary<'_>, rules: &[Rule]) -> String {
    let mut o = format!(
        "{START}\n## Review rules\n\nRecurring expectations from this repository's code review history, \
found by [difflore]({HOME}) in {} maintainer comments across {} pull requests. Each rule links to \
the reviews it came from.\n\n",
        summary.comments, summary.prs
    );
    for r in rules {
        let _ = write!(o, "- **{}**", r.rule);
        if !r.why.is_empty() {
            let _ = write!(o, " {}", r.why);
        }
        if !r.paths.is_empty() {
            let _ = write!(o, " Applies to `{}`.", r.paths);
        }
        let links: Vec<String> = r
            .evidence
            .iter()
            .take(3)
            .map(|e| format!("[#{}]({})", e.pr, e.url))
            .collect();
        let _ = writeln!(o, " ({})", links.join(", "));
    }
    o.push_str(END);
    o.push('\n');
    o
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Agents,
    Claude,
    Cursor,
}

pub fn write(target: Target, dir: &Path, summary: &Summary<'_>, rules: &[Rule]) -> Result<PathBuf> {
    let block = markdown_block(summary, rules);
    let path = match target {
        Target::Agents => dir.join("AGENTS.md"),
        Target::Claude => dir.join("CLAUDE.md"),
        Target::Cursor => dir.join(".cursor/rules/difflore.mdc"),
    };
    let content = if target == Target::Cursor {
        format!(
            "---\ndescription: Review rules mined from {} code review history\nalwaysApply: true\n---\n\n{block}",
            summary.repo
        )
    } else {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        splice(&existing, &block)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Replaces an earlier difflore block in place, or appends one.
fn splice(existing: &str, block: &str) -> String {
    if let (Some(s), Some(e)) = (existing.find(START), existing.find(END))
        && s < e
    {
        let after = &existing[e + END.len()..];
        let after = after.strip_prefix('\n').unwrap_or(after);
        return format!("{}{block}{after}", &existing[..s]);
    }
    if existing.trim().is_empty() {
        return block.to_owned();
    }
    let sep = if existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{existing}{sep}{block}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_replaces_old_block_and_keeps_the_rest() {
        let block = format!("{START}\nnew\n{END}\n");
        let file = format!("# Title\n\n{START}\nold\n{END}\n\n## Other\n");
        assert_eq!(
            splice(&file, &block),
            format!("# Title\n\n{block}\n## Other\n")
        );
    }

    #[test]
    fn splice_appends_after_existing_content() {
        let block = format!("{START}\nnew\n{END}\n");
        assert_eq!(splice("# Title", &block), format!("# Title\n\n{block}"));
        assert_eq!(splice("", &block), block);
    }
}
