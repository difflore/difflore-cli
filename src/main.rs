mod agent;
mod distill;
mod github;
mod render;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;

use agent::{Agent, AgentKind};
use github::{FetchOptions, Repo};
use render::{Summary, Target};

/// Find the unwritten rules in a GitHub repo's code review history.
///
/// difflore reads the review comments maintainers left on pull requests, asks your
/// own coding agent CLI (Claude Code, Codex or Pi) which expectations keep coming up,
/// and prints them with links to the reviews they came from. Add --write to save
/// them as AGENTS.md, CLAUDE.md or a Cursor rule.
#[derive(Parser)]
#[command(name = "difflore", version, max_term_width = 100)]
struct Cli {
    /// GitHub repo as owner/name or URL. Defaults to this directory's `origin`.
    repo: Option<String>,

    /// Save the rules: agents (AGENTS.md), claude (CLAUDE.md) or cursor (.cursor/rules).
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "agents")]
    write: Option<Target>,

    /// Directory to write into.
    #[arg(long, default_value = ".")]
    dir: PathBuf,

    /// Print the rules as JSON instead.
    #[arg(long)]
    json: bool,

    /// Most recent maintainer review comments to read.
    #[arg(long, default_value_t = 300)]
    limit: usize,

    /// Maximum rules to keep.
    #[arg(long, default_value_t = 15)]
    rules: usize,

    /// Keep only rules seen in at least this many pull requests.
    #[arg(long, default_value_t = 2)]
    min_prs: usize,

    /// Agent CLI to think with. Defaults to the first of claude, codex, pi on PATH.
    #[arg(long, value_enum, env = "DIFFLORE_AGENT")]
    agent: Option<AgentKind>,

    /// Model passed to the agent CLI.
    #[arg(long, env = "DIFFLORE_MODEL")]
    model: Option<String>,

    /// Agent calls to run at once.
    #[arg(long, default_value_t = 4)]
    jobs: usize,

    /// Include comments from every reviewer, not only owners, members and collaborators.
    #[arg(long)]
    all_reviewers: bool,

    /// Refetch comments instead of using the 24-hour cache.
    #[arg(long)]
    refresh: bool,
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("difflore: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let repo = match &cli.repo {
        Some(r) => Repo::parse(r).with_context(|| format!("not a GitHub repo: {r}"))?,
        None => Repo::from_git_remote()?,
    };
    let kind = cli
        .agent
        .or_else(AgentKind::detect)
        .context("no agent CLI found; install Claude Code, Codex or Pi, or pass --agent")?;
    let agent = Agent {
        kind,
        model: cli.model.clone(),
    };
    let progress = |msg: &str| eprintln!("  {msg}");

    let comments = github::fetch_comments(
        &repo,
        &FetchOptions {
            limit: cli.limit,
            all_reviewers: cli.all_reviewers,
            refresh: cli.refresh,
        },
        &progress,
    )?;
    if comments.is_empty() {
        bail!("{repo} has no maintainer review comments to read (try --all-reviewers)");
    }
    let prs = comments.iter().map(|c| c.pr).collect::<BTreeSet<_>>().len();
    progress(&format!(
        "Reading {} comments from {prs} PRs with {}",
        comments.len(),
        agent.label()
    ));

    let rules = distill::distill(
        &repo,
        &comments,
        &agent,
        &distill::Options {
            max_rules: cli.rules,
            min_prs: cli.min_prs,
            jobs: cli.jobs,
        },
        &progress,
    )?;

    let summary = Summary {
        repo: &repo,
        comments: comments.len(),
        prs,
    };
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&rules)?);
    } else {
        print!("{}", render::terminal(&summary, &rules));
    }
    if let Some(target) = cli.write {
        if rules.is_empty() {
            bail!("nothing to write");
        }
        let path = render::write(target, &cli.dir, &summary, &rules)?;
        eprintln!("Wrote {}", path.display());
    } else if !cli.json && !rules.is_empty() {
        eprintln!("Save them with: difflore {repo} --write   (or --write claude / cursor)");
    }
    Ok(())
}
