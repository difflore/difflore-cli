use serde_json::{Value, json};

use crate::context::retrieval;
use crate::context::types::{PastVerdict, PastVerdictScope};
use crate::observability::trajectory::TrajectoryStep;

use super::super::{
    McpState, build_cost_meta, detect_git_remote_owner_repos, emit_trajectory_step, estimate_tokens,
};
use super::validate::{MCP_TEXT_ARG_CHAR_LIMIT, validate_mcp_text_arg};

pub(crate) async fn tool_get_past_verdicts(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "Missing required parameter: query".to_owned()))?;
    validate_mcp_text_arg("query", query, MCP_TEXT_ARG_CHAR_LIMIT)?;
    // Optional `file` enables server-side cascade ordering.
    let target_file = args
        .get("file")
        .and_then(|v| v.as_str())
        .and_then(normalize_target_file_for_cloud);
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(10, |n| n.clamp(1, 10) as usize);

    // Scope to explicit or detected repos. On detection failure, return
    // no verdicts rather than widening to other projects.
    let explicit_repo = args
        .get("repo_full_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let repo_scopes: Vec<String> = if let Some(repo) = explicit_repo {
        vec![repo]
    } else {
        // Warm the configured-GitLab-host cache before detecting remotes so a
        // fresh MCP-server process that calls get_past_verdicts before any
        // recall can still resolve self-managed GitLab scopes. Without it the
        // detection falls empty and the verdict recall silently returns nothing.
        // Mirrors hook.rs / search_rules.rs / remember_rule.rs.
        crate::mcp_server::hook::refresh_configured_gitlab_hosts_for_remote_detection().await;
        detect_git_remote_owner_repos()
    };

    // Retain per-repo provenance before merging fan-out responses.
    let mut repo_by_extraction: std::collections::HashMap<String, (String, f32)> =
        std::collections::HashMap::new();
    let cloud = &state.cloud;
    let cloud_status = crate::cloud::sync::fetch_cloud_status(cloud).await;
    if !cloud_status.logged_in {
        let text = "Cloud PR review rules skipped: not logged in. Local recall still works offline; run `difflore cloud login` to append imported PR review rules.";
        let tokens_used = estimate_tokens(text);
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "_meta": {
                "cost": build_cost_meta(tokens_used, None),
                "loggedIn": false,
                "recallScope": PastVerdictScope::Personal.as_str(),
                "teamId": Option::<String>::None,
                "impact": { "verdictsRecalled": 0, "kind": "verdicts" }
            }
        }));
    }
    let team_id = cloud_status.team_id.clone();
    let scope = if team_id.is_some() {
        PastVerdictScope::Team
    } else {
        PastVerdictScope::Personal
    };
    // Cap fan-out at 4 repos to bound MCP latency.
    let recalls = repo_scopes.into_iter().take(4).map(|repo| {
        let target_file = target_file.clone();
        let team_id = team_id.clone();
        async move {
            let group = retrieval::retrieve_past_verdicts_by_text_with_team(
                cloud,
                query,
                Some(repo.as_str()),
                scope,
                u32::try_from(top_k).unwrap_or(10),
                target_file.as_deref(),
                team_id.as_deref(),
            )
            .await;
            (repo, group)
        }
    });

    let mut groups = Vec::new();
    for (repo, group) in futures_util::future::join_all(recalls).await {
        for v in &group {
            let entry = repo_by_extraction
                .entry(v.extraction_id.clone())
                .or_insert_with(|| (repo.clone(), v.similarity));
            if v.similarity > entry.1 {
                *entry = (repo.clone(), v.similarity);
            }
        }
        groups.push(group);
    }
    let verdicts: Vec<PastVerdict> = retrieval::merge_past_verdicts(groups, top_k);

    if verdicts.is_empty() {
        let decision_scope = if team_id.is_some() {
            "team decisions"
        } else {
            "personal decisions"
        };
        let text = format!(
            "No past verdicts found for this query.\n\n> No matching cloud {decision_scope} yet. Import past GitHub reviews with `difflore import-reviews`, then run `difflore status` and `difflore recall --diff` to verify the recall path."
        );
        let tokens_used = estimate_tokens(&text);
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "_meta": {
                "cost": build_cost_meta(tokens_used, None),
                "recallScope": scope.as_str(),
                "teamId": team_id,
                "impact": { "verdictsRecalled": 0, "kind": "verdicts" }
            }
        }));
    }

    // Format verdicts into readable text, including repo provenance when
    // available.
    let mut text = String::from("## Past Review Verdicts\n\n");
    for (i, v) in verdicts.iter().enumerate() {
        let source_repo = repo_by_extraction
            .get(&v.extraction_id)
            .map(|(repo, _)| repo.as_str());
        let provenance = source_pr_label(v, source_repo)
            .or_else(|| source_repo.map(str::to_owned))
            .map(|source| format!(" <- from {source}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "### {} [{}, similarity {:.2}]{}\n\n",
            i + 1,
            v.status,
            v.similarity,
            provenance,
        ));
        text.push_str(&format!("**Code:**\n```\n{}\n```\n\n", v.code_snippet));
        text.push_str(&format!("**Issue:** {}\n", v.issue_text));
        if let Some(reason) = v.reason.as_ref()
            && !reason.is_empty()
        {
            text.push_str(&format!("**Reason:** {reason}\n"));
        }
        if let Some(line) = source_pr_line(v, source_repo) {
            text.push_str(&format!("{line}\n"));
        }
        text.push('\n');
    }

    let n = verdicts.len();
    let top_sim = verdicts.first().map_or(0.0, |v| v.similarity);
    let source_pr_linked = verdicts
        .iter()
        .filter(|v| {
            let source_repo = repo_by_extraction
                .get(&v.extraction_id)
                .map(|(repo, _)| repo.as_str());
            source_pr_label(v, source_repo).is_some()
        })
        .count();
    let any_repo_attributed = verdicts
        .iter()
        .any(|v| repo_by_extraction.contains_key(&v.extraction_id));
    let cite_hint = if source_pr_linked > 0 {
        " If you follow one, cite it AND its source PR (e.g. \"past verdict #1 from gin-gonic/gin#4336 applies here\") so the user sees which real review shaped this change."
    } else if any_repo_attributed {
        " If you follow one, cite it AND its source repo (e.g. \"past verdict #1 from gin-gonic/gin applies here\") so the user sees which past team judgment shaped this change."
    } else {
        " If you follow one, cite it (e.g. \"past verdict #1 applies here\") so the user sees which knowledge shaped this change."
    };
    text.push_str(&format!(
        "\n> **DiffLore recalled {} past team decision{}** (top similarity {:.2}).{}",
        n,
        if n == 1 { "" } else { "s" },
        top_sim,
        cite_hint,
    ));

    // Track MCP response size; past verdicts are not injected rules.
    let tokens_used = estimate_tokens(&text);
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "get_past_verdicts".to_owned(),
        total_tokens: tokens_used,
        rules_injected: 0,
    });

    Ok(json!({
        "content": [{
            "type": "text",
            "text": text.trim_end()
        }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, None),
            "recallScope": scope.as_str(),
            "teamId": team_id,
            "impact": {
                "verdictsRecalled": n,
                "kind": "verdicts",
                "topSimilarity": top_sim,
                "sourcePrLinked": source_pr_linked,
            }
        }
    }))
}

fn source_pr_line(verdict: &PastVerdict, source_repo: Option<&str>) -> Option<String> {
    let label = source_pr_label(verdict, source_repo)?;
    let mut line = format!("**Source PR:** {label}");

    if let Some(title) = verdict.source_pr_title.as_deref().map(str::trim)
        && !title.is_empty()
    {
        line.push_str(&format!(" - {title}"));
    }

    if let Some(url) = verdict.source_pr_url.as_deref().map(str::trim)
        && !url.is_empty()
    {
        line.push_str(&format!(" ({url})"));
    }

    Some(line)
}

fn source_pr_label(verdict: &PastVerdict, source_repo: Option<&str>) -> Option<String> {
    let parsed = verdict
        .source_pr_url
        .as_deref()
        .and_then(parse_github_pr_url);

    if let Some(number) = verdict.source_pr_number {
        let repo = source_repo
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
            .or_else(|| parsed.as_ref().map(|(repo, _)| repo.clone()));

        return Some(match repo {
            Some(repo) => format!("{repo}#{number}"),
            None => format!("PR #{number}"),
        });
    }

    if let Some((repo, Some(number))) = parsed {
        return Some(format!("{repo}#{number}"));
    }

    verdict
        .source_pr_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}

fn parse_github_pr_url(url: &str) -> Option<(String, Option<i64>)> {
    let rest = url.split_once("github.com/")?.1;
    let mut parts = rest.trim_matches('/').split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let kind = parts.next();
    let number = parts.next().and_then(|n| n.parse::<i64>().ok());
    let pr_number = if matches!(kind, Some("pull" | "pulls")) {
        number
    } else {
        None
    };

    Some((format!("{owner}/{repo}"), pr_number))
}

fn normalize_target_file_for_cloud(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "unknown" {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        let path = std::path::Path::new(trimmed);
        if path.is_absolute()
            && let Ok(relative) = path.strip_prefix(cwd)
        {
            return path_components_for_cloud(relative, usize::MAX);
        }
    }
    let path = std::path::Path::new(&normalized);
    let max_components = if path.is_absolute() || has_windows_drive_prefix(&normalized) {
        4
    } else {
        usize::MAX
    };
    path_components_for_cloud(path, max_components)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    path.as_bytes().get(1) == Some(&b':')
}

fn path_components_for_cloud(path: &std::path::Path, max_components: usize) -> Option<String> {
    let mut parts: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .filter(|part| part != "." && part != ".." && !part.trim().is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() > max_components {
        parts = parts.split_off(parts.len() - max_components);
    }
    let joined = parts.join("/");
    Some(joined.chars().take(512).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict() -> PastVerdict {
        PastVerdict {
            extraction_id: "e1".to_owned(),
            code_snippet: "snippet".to_owned(),
            issue_text: "issue".to_owned(),
            status: "accepted".to_owned(),
            reason: Some("reason".to_owned()),
            similarity: 0.91,
            created_at: "2026-04-10T00:00:00Z".to_owned(),
            signature: None,
            source_pr_number: Some(4336),
            source_pr_title: Some("fix recovery panic handling".to_owned()),
            source_pr_url: Some("https://github.com/gin-gonic/gin/pull/4336".to_owned()),
        }
    }

    #[test]
    fn source_pr_label_prefers_recall_repo_with_number() {
        assert_eq!(
            source_pr_label(&verdict(), Some("upstream/fork")),
            Some("upstream/fork#4336".to_owned())
        );
    }

    #[test]
    fn source_pr_line_includes_exact_pr_title_and_url() {
        assert_eq!(
            source_pr_line(&verdict(), Some("gin-gonic/gin")),
            Some("**Source PR:** gin-gonic/gin#4336 - fix recovery panic handling (https://github.com/gin-gonic/gin/pull/4336)".to_owned())
        );
    }

    #[test]
    fn source_pr_label_can_parse_github_pr_url_without_number_field() {
        let mut verdict = verdict();
        verdict.source_pr_number = None;

        assert_eq!(
            source_pr_label(&verdict, None),
            Some("gin-gonic/gin#4336".to_owned())
        );
    }

    #[test]
    fn normalize_target_file_strips_absolute_prefix() {
        let path = if cfg!(windows) {
            r"C:\Users\alice\workspace\crates\difflore-core\src\lib.rs"
        } else {
            "/Users/alice/workspace/crates/difflore-core/src/lib.rs"
        };

        let normalized = normalize_target_file_for_cloud(path).expect("normalized path");

        assert_eq!(normalized, "crates/difflore-core/src/lib.rs");
        assert!(!normalized.contains("alice"));
        assert!(!normalized.contains("workspace"));
    }

    #[test]
    fn normalize_target_file_keeps_relative_path() {
        assert_eq!(
            normalize_target_file_for_cloud("crates/difflore-core/src/lib.rs"),
            Some("crates/difflore-core/src/lib.rs".to_owned())
        );
    }
}
