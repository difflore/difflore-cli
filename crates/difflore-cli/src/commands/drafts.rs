//! Compatibility surface for local memory draft review.

use std::io::{self, BufRead, IsTerminal, Write};

use serde_json::json;

use difflore_core::memory_autopilot::promote_candidate_with_curator_recommendation;
use difflore_core::skills::{CandidateRule, list_candidates, reject_candidate};

use crate::style;
use crate::support::util::{
    confirm_destructive, exit_code, exit_err, init_db, json_compact_or, validate_owner_repo,
};

pub(crate) async fn handle_list(
    repo: Option<String>,
    limit: Option<usize>,
    source_kind: Option<String>,
    json: bool,
) {
    validate_repo_arg(repo.as_deref());
    let source_kind = normalize_source_kind_filter(source_kind.as_deref(), json);
    let db = init_db().await;
    let drafts =
        load_filtered_drafts(&db, repo.as_deref(), limit, source_kind.as_deref(), json).await;

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "count": drafts.len(),
                    "repo": repo,
                    "sourceKind": source_kind,
                    "drafts": drafts,
                }),
                "{}"
            )
        );
        return;
    }

    if drafts.is_empty() {
        println!("No pending memory drafts.");
        return;
    }

    println!("Pending memory drafts ({}):\n", drafts.len());
    for draft in &drafts {
        print_draft_summary(draft);
    }
    println!(
        "\n  {} review interactively with {}",
        style::emerald(style::sym::TIP),
        style::cmd("difflore memory review")
    );
}

pub(crate) async fn handle_show(id: String, json: bool) {
    let db = init_db().await;
    let draft = load_draft_by_id(&db, &id, json).await;

    if json {
        println!("{}", json_compact_or(&draft, "{}"));
        return;
    }

    print_draft_full(&draft);
}

pub(crate) async fn handle_review(
    repo: Option<String>,
    limit: Option<usize>,
    source_kind: Option<String>,
) {
    validate_repo_arg(repo.as_deref());
    let source_kind = normalize_source_kind_filter(source_kind.as_deref(), false);
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        exit_err(
            "interactive draft review requires a terminal. Prefer `difflore memory inbox`, \
             `difflore memory approve draft:<id>`, or \
             `difflore memory reject draft:<id>`.",
        );
    }

    let db = init_db().await;
    let drafts =
        load_filtered_drafts(&db, repo.as_deref(), limit, source_kind.as_deref(), false).await;
    if drafts.is_empty() {
        println!("No pending memory drafts.");
        return;
    }

    println!("Reviewing {} pending memory draft(s).\n", drafts.len());
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    for (idx, draft) in drafts.iter().enumerate() {
        println!(
            "{} of {}",
            style::pewter(&(idx + 1).to_string()),
            style::pewter(&drafts.len().to_string())
        );
        print_draft_summary(draft);

        loop {
            print!(
                "  action [{} approve, {} reject, {} skip, {} view, {} quit]: ",
                style::cmd("a"),
                style::cmd("r"),
                style::cmd("s"),
                style::cmd("v"),
                style::cmd("q"),
            );
            flush_stdout();

            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                exit_err("failed to read draft review choice");
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "a" | "approve" => {
                    match promote_candidate_with_curator_recommendation(&db, &draft.id).await {
                        Ok(_) => {
                            println!("  {} approved {}\n", style::ok(style::sym::OK), draft.id);
                        }
                        Err(e) => exit_err(&format!("failed to approve draft `{}`: {e}", draft.id)),
                    }
                    break;
                }
                "r" | "reject" => {
                    match reject_candidate(&db, &draft.id).await {
                        Ok(()) => {
                            println!("  {} rejected {}\n", style::ok(style::sym::OK), draft.id);
                        }
                        Err(e) => exit_err(&format!("failed to reject draft `{}`: {e}", draft.id)),
                    }
                    break;
                }
                "s" | "skip" | "" => {
                    println!("  skipped {}\n", draft.id);
                    break;
                }
                "v" | "view" => print_draft_full(draft),
                "q" | "quit" => {
                    println!("Stopped with remaining drafts still pending.");
                    return;
                }
                _ => {
                    println!("  enter a, r, s, v, or q.");
                }
            }
        }
    }
}

pub(crate) async fn handle_approve(
    id: Option<String>,
    all: bool,
    repo: Option<String>,
    yes: bool,
    json: bool,
) {
    validate_bulk_args("approve", id.as_deref(), all, repo.as_deref(), json);
    let db = init_db().await;

    if all {
        let drafts = load_drafts(&db, repo.as_deref(), None, json).await;
        confirm_non_empty_bulk("approve", &drafts, repo.as_deref(), json);
        if let Err(e) = confirm_destructive(
            yes,
            &format!("approve {} pending memory draft(s)?", drafts.len()),
        ) {
            exit_structured_err(&format!("{e:#}"), json);
        }
        let mut approved = Vec::new();
        for draft in drafts {
            promote_candidate_with_curator_recommendation(&db, &draft.id)
                .await
                .unwrap_or_else(|e| exit_action_err("approve", &draft.id, &e, json));
            approved.push(draft.id);
        }
        print_action_result("approved", &approved, json);
        return;
    }

    let Some(id) = id else {
        exit_structured_err(
            "missing draft id. Use `difflore drafts approve <id>` or `--all`.",
            json,
        );
    };
    let activated = promote_candidate_with_curator_recommendation(&db, &id)
        .await
        .unwrap_or_else(|e| exit_action_err("approve", &id, &e, json));
    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "approved",
                    "count": 1,
                    "ids": [activated.id],
                }),
                "{}"
            )
        );
    } else {
        println!(
            "{} Approved memory draft {}.",
            style::ok(style::sym::OK),
            style::ident(&activated.id)
        );
    }
}

pub(crate) async fn handle_reject(
    id: Option<String>,
    all: bool,
    repo: Option<String>,
    yes: bool,
    json: bool,
) {
    validate_bulk_args("reject", id.as_deref(), all, repo.as_deref(), json);
    let db = init_db().await;

    if all {
        let drafts = load_drafts(&db, repo.as_deref(), None, json).await;
        confirm_non_empty_bulk("reject", &drafts, repo.as_deref(), json);
        if let Err(e) = confirm_destructive(
            yes,
            &format!("reject {} pending memory draft(s)?", drafts.len()),
        ) {
            exit_structured_err(&format!("{e:#}"), json);
        }
        let mut rejected = Vec::new();
        for draft in drafts {
            reject_candidate(&db, &draft.id)
                .await
                .unwrap_or_else(|e| exit_action_err("reject", &draft.id, &e, json));
            rejected.push(draft.id);
        }
        print_action_result("rejected", &rejected, json);
        return;
    }

    let Some(id) = id else {
        exit_structured_err(
            "missing draft id. Use `difflore drafts reject <id>` or `--all`.",
            json,
        );
    };
    reject_candidate(&db, &id)
        .await
        .unwrap_or_else(|e| exit_action_err("reject", &id, &e, json));
    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "rejected",
                    "count": 1,
                    "ids": [id],
                }),
                "{}"
            )
        );
    } else {
        println!(
            "{} Rejected memory draft {}.",
            style::ok(style::sym::OK),
            style::ident(&id)
        );
    }
}

async fn load_drafts(
    db: &difflore_core::SqlitePool,
    repo: Option<&str>,
    limit: Option<usize>,
    json: bool,
) -> Vec<CandidateRule> {
    list_candidates(db, repo, limit).await.unwrap_or_else(|e| {
        exit_structured_err(&format!("failed to list pending memory drafts: {e}"), json)
    })
}

async fn load_filtered_drafts(
    db: &difflore_core::SqlitePool,
    repo: Option<&str>,
    limit: Option<usize>,
    source_kind: Option<&str>,
    json: bool,
) -> Vec<CandidateRule> {
    let initial_limit = if source_kind.is_some() { None } else { limit };
    let mut drafts = load_drafts(db, repo, initial_limit, json).await;
    if let Some(source_kind) = source_kind {
        drafts.retain(|draft| source_kind_matches(&draft.source_kind, source_kind));
        if let Some(limit) = limit {
            drafts.truncate(limit);
        }
    }
    drafts
}

async fn load_draft_by_id(db: &difflore_core::SqlitePool, id: &str, json: bool) -> CandidateRule {
    list_candidates(db, None, None)
        .await
        .unwrap_or_else(|e| {
            exit_structured_err(&format!("failed to list pending memory drafts: {e}"), json)
        })
        .into_iter()
        .find(|draft| draft.id == id)
        .unwrap_or_else(|| exit_structured_err(&format!("memory draft `{id}` not found"), json))
}

fn normalize_source_kind_filter(raw: Option<&str>, json: bool) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        exit_structured_err("--source-kind must not be empty", json);
    }
    if raw == "human" || raw == "human_override_bot" || raw == "bot" || raw == "bot:*" {
        return Some(raw.to_owned());
    }
    if let Some(bot) = raw.strip_prefix("bot:")
        && !bot.trim().is_empty()
    {
        return Some(raw.to_owned());
    }
    exit_structured_err(
        "--source-kind must be human, human_override_bot, bot, bot:*, or bot:<name>",
        json,
    );
}

fn source_kind_matches(value: &str, filter: &str) -> bool {
    match filter {
        "bot" | "bot:*" => value.starts_with("bot:"),
        _ => value == filter,
    }
}

fn validate_repo_arg(repo: Option<&str>) {
    if let Some(repo) = repo
        && let Err(msg) = validate_owner_repo(repo)
    {
        exit_err(&format!("--repo '{repo}' is invalid: {msg}"));
    }
}

fn validate_bulk_args(action: &str, id: Option<&str>, all: bool, repo: Option<&str>, json: bool) {
    validate_repo_arg(repo);
    match (id, all) {
        (Some(_), false) if repo.is_some() => exit_structured_err(
            &format!("--repo only applies to `difflore drafts {action} --all`"),
            json,
        ),
        (Some(_), false) | (None, true) => {}
        (Some(_), true) => exit_structured_err("pass either a draft id or --all, not both", json),
        (None, false) => exit_structured_err(
            &format!("missing draft id. Use `difflore drafts {action} <id>` or `--all`."),
            json,
        ),
    }
}

fn confirm_non_empty_bulk(action: &str, drafts: &[CandidateRule], repo: Option<&str>, json: bool) {
    if !drafts.is_empty() {
        return;
    }
    let scope = repo.map_or("all repos".to_owned(), |repo| format!("repo {repo}"));
    exit_structured_err(
        &format!("no pending memory drafts to {action} for {scope}"),
        json,
    );
}

fn print_action_result(action: &str, ids: &[String], json: bool) {
    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": action,
                    "count": ids.len(),
                    "ids": ids,
                }),
                "{}"
            )
        );
        return;
    }
    println!(
        "{} {} {} memory draft(s).",
        style::ok(style::sym::OK),
        capitalize(action),
        ids.len()
    );
}

fn print_draft_summary(draft: &CandidateRule) {
    println!("  {} {}", style::ident(&draft.id), draft.name);
    println!(
        "    origin={}  source_kind={}  source_repo={}  captured={}",
        draft.origin,
        draft.source_kind,
        draft.source_repo.as_deref().unwrap_or("-"),
        draft.installed_at
    );
    if !draft.file_patterns.is_empty() {
        println!("    globs={}", draft.file_patterns.join(", "));
    }
    if let Some(proof) = &draft.source_proof {
        if let Some(source) = &proof.source {
            println!("    source={source}");
        }
        if let Some(file) = &proof.file {
            println!("    file={file}");
        }
    }
    println!("    {}", truncate_chars(&draft_preview(draft), 180));
}

fn print_draft_full(draft: &CandidateRule) {
    println!("\n{}", style::ident(&draft.name));
    println!("  id: {}", draft.id);
    println!("  origin: {}", draft.origin);
    println!("  source_kind: {}", draft.source_kind);
    println!(
        "  source_repo: {}",
        draft.source_repo.as_deref().unwrap_or("-")
    );
    println!("  captured: {}", draft.installed_at);
    if !draft.file_patterns.is_empty() {
        println!("  globs: {}", draft.file_patterns.join(", "));
    }
    if let Some(proof) = &draft.source_proof {
        if let Some(source) = &proof.source {
            println!("  source: {source}");
        }
        if let Some(comment_url) = &proof.comment_url {
            println!("  comment: {comment_url}");
        }
        if let Some(file) = &proof.file {
            println!("  file: {file}");
        }
        if let Some(excerpt) = &proof.excerpt {
            println!("\n  reviewer said:\n{}", indent_block(excerpt, "    "));
        }
    }
    println!(
        "\n  drafted rule:\n{}\n",
        indent_block(&draft_rule_text(draft), "    ")
    );
}

fn draft_preview(draft: &CandidateRule) -> String {
    draft
        .drafted_rule
        .as_deref()
        .or_else(|| {
            draft
                .source_proof
                .as_ref()
                .and_then(|proof| proof.excerpt.as_deref())
        })
        .map_or_else(
            || first_nonempty_line(&draft.description),
            ToOwned::to_owned,
        )
}

fn draft_rule_text(draft: &CandidateRule) -> String {
    draft
        .drafted_rule
        .clone()
        .unwrap_or_else(|| draft.description.clone())
}

fn first_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn indent_block(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn flush_stdout() {
    if let Err(e) = io::stdout().flush() {
        exit_err(&format!("failed to write prompt: {e}"));
    }
}

fn exit_action_err(action: &str, id: &str, error: &difflore_core::CoreError, json: bool) -> ! {
    exit_structured_err(
        &format!("failed to {action} memory draft `{id}`: {error}"),
        json,
    )
}

fn exit_structured_err(message: &str, json: bool) -> ! {
    if json {
        println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
        exit_code(1);
    }
    exit_err(message);
}

#[cfg(test)]
mod tests {
    use super::source_kind_matches;

    #[test]
    fn source_kind_filter_matches_exact_and_bot_wildcard() {
        assert!(source_kind_matches("human", "human"));
        assert!(source_kind_matches(
            "human_override_bot",
            "human_override_bot"
        ));
        assert!(source_kind_matches("bot:coderabbitai[bot]", "bot:*"));
        assert!(source_kind_matches("bot:coderabbitai[bot]", "bot"));
        assert!(source_kind_matches(
            "bot:coderabbitai[bot]",
            "bot:coderabbitai[bot]"
        ));
        assert!(!source_kind_matches("human", "bot:*"));
        assert!(!source_kind_matches(
            "bot:reviewdog[bot]",
            "bot:coderabbitai[bot]"
        ));
    }
}
