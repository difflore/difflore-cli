use std::collections::HashSet;
use std::time::{Duration, Instant};

use difflore_core::domain::models::RememberRuleInput;
use difflore_core::infra::git::RepoScope;
use difflore_core::review_store::{self, ReviewItemWithComments};
use sqlx::SqlitePool;

use crate::agent_exec::{AgentKind, GateResult, dispatch_gate};

use super::local_candidates::{
    CAPTURE_CONFIDENCE_LOW, LocalCandidateProgress, local_candidate_budget_reached,
    local_candidate_dedupe_signature, local_candidate_input,
};

const LOCAL_AGENT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);
const LOCAL_AGENT_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const LOCAL_AGENT_CANDIDATE_CONFIDENCE: f32 = CAPTURE_CONFIDENCE_LOW;
const LOCAL_AGENT_PROMPT_MAX_SEEDS: usize = 24;

const LOCAL_AGENT_DISTILL_AGENTS: [AgentKind; 3] = [
    AgentKind::ClaudeCode,
    AgentKind::GeminiCli,
    AgentKind::Codex,
];

#[derive(Debug)]
pub(super) struct LocalAgentDistillError {
    message: String,
}

impl std::fmt::Display for LocalAgentDistillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LocalAgentDistillError {}

#[derive(Debug, Clone)]
struct DistillSeed {
    index: usize,
    input: RememberRuleInput,
}

#[derive(Debug, serde::Deserialize)]
struct AgentDistillEnvelope {
    #[serde(default)]
    candidates: Vec<AgentDistillCandidate>,
}

#[derive(Debug, serde::Deserialize)]
struct AgentDistillCandidate {
    source_index: Option<usize>,
    title: Option<String>,
    body: Option<String>,
    #[serde(default)]
    file_patterns: Vec<String>,
}

pub(super) async fn run_local_agent_candidates(
    db: &SqlitePool,
    source: &str,
    repo: &str,
    source_repo: &RepoScope,
    max_candidates: usize,
    pr_numbers: &[i32],
    exclude_prs: &HashSet<i32>,
) -> Result<LocalCandidateProgress, LocalAgentDistillError> {
    let items = review_store::list_by_source_with_comments(
        db,
        review_store::ReviewSourceInput {
            source: source.into(),
        },
    )
    .await
    .map_err(|e| distill_error(format!("failed to load imported reviews: {e}")))?;

    let (seeds, mut progress) = collect_distill_seeds(
        &items,
        repo,
        source_repo,
        max_candidates,
        pr_numbers,
        exclude_prs,
    );
    if seeds.is_empty() {
        return Ok(progress);
    }

    let prompt = build_distill_prompt(&seeds);
    let stdout = dispatch_local_agent_distill(&prompt).await?;
    let candidates = parse_distill_candidates(&stdout)?;
    write_agent_candidates(db, source_repo, &seeds, candidates, &mut progress).await?;
    Ok(progress)
}

fn collect_distill_seeds(
    items: &[ReviewItemWithComments],
    repo: &str,
    source_repo: &RepoScope,
    max_candidates: usize,
    pr_numbers: &[i32],
    exclude_prs: &HashSet<i32>,
) -> (Vec<DistillSeed>, LocalCandidateProgress) {
    let target_pr_numbers = pr_numbers.iter().copied().collect::<HashSet<_>>();
    let mut progress = LocalCandidateProgress {
        budget: max_candidates,
        ..LocalCandidateProgress::default()
    };
    let mut seeds = Vec::new();
    let mut next_index = 1;

    'items: for item in items
        .iter()
        .filter(|item| item.item.repo_full_name.as_deref() == Some(repo))
        .filter(|item| {
            item.item
                .pr_number
                .is_none_or(|n| !exclude_prs.contains(&n))
        })
        .filter(|item| {
            target_pr_numbers.is_empty()
                || item
                    .item
                    .pr_number
                    .is_some_and(|n| target_pr_numbers.contains(&n))
        })
    {
        for comment in &item.comments {
            progress.comments_considered += 1;
            let Some(local_candidate) = local_candidate_input(item, comment, source_repo) else {
                progress.comments_skipped += 1;
                continue;
            };
            seeds.push(DistillSeed {
                index: next_index,
                input: local_candidate.input,
            });
            next_index += 1;
            if seeds.len() >= max_candidates.min(LOCAL_AGENT_PROMPT_MAX_SEEDS) {
                progress.capped = seeds.len() >= max_candidates;
                break 'items;
            }
        }
    }

    (seeds, progress)
}

fn build_distill_prompt(seeds: &[DistillSeed]) -> String {
    let mut out = String::from(
        "You are distilling imported PR review comments into pending DiffLore rule candidates.\n\
         Use only the supplied review evidence. Keep reusable, non-obvious coding rules.\n\
         Improve wording and merge away duplicates, but do not invent facts.\n\
         Return STRICT JSON only, no markdown:\n\
         {\"candidates\":[{\"source_index\":1,\"title\":\"...\",\"body\":\"Rule:\\n...\\n\\nSource evidence:\\n...\",\"file_patterns\":[\"src/**/*.ts\"]}]}\n\
         If nothing is reusable, return {\"candidates\":[]}; the CLI will fall back to deterministic heuristics.\n\n",
    );
    out.push_str("REVIEW CANDIDATE SEEDS:\n");
    for seed in seeds {
        out.push_str(&format!(
            "\nSOURCE_INDEX: {}\nTITLE: {}\nFILE_PATTERNS: {}\nBODY:\n{}\n",
            seed.index,
            truncate_chars(&seed.input.title, 240),
            seed.input
                .file_patterns
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), |patterns| patterns.join(", ")),
            truncate_chars(&seed.input.body, 2_000),
        ));
    }
    out
}

async fn dispatch_local_agent_distill(prompt: &str) -> Result<String, LocalAgentDistillError> {
    let started = Instant::now();
    let mut errors = Vec::new();
    for agent in LOCAL_AGENT_DISTILL_AGENTS {
        let Some(budget) = local_agent_budget(started.elapsed()) else {
            errors.push(format!(
                "time budget exhausted after {}s",
                LOCAL_AGENT_TOTAL_TIMEOUT.as_secs()
            ));
            break;
        };
        let result: GateResult = dispatch_gate(agent, prompt, budget).await;
        if result.errored {
            errors.push(format!(
                "{}: {}",
                agent.label(),
                if result.error_message.is_empty() {
                    "agent CLI reported error with no message"
                } else {
                    result.error_message.as_str()
                }
            ));
            continue;
        }
        return Ok(result.stdout);
    }

    Err(distill_error(format!(
        "all local-agent distillers failed: {}",
        errors.join("; ")
    )))
}

fn local_agent_budget(elapsed: Duration) -> Option<Duration> {
    let remaining = LOCAL_AGENT_TOTAL_TIMEOUT.checked_sub(elapsed)?;
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.min(LOCAL_AGENT_ATTEMPT_TIMEOUT))
}

fn parse_distill_candidates(
    stdout: &str,
) -> Result<Vec<AgentDistillCandidate>, LocalAgentDistillError> {
    let json = extract_json_payload(stdout).ok_or_else(|| {
        distill_error(format!(
            "local-agent distill returned no JSON object: {}",
            truncate_chars(stdout, 300)
        ))
    })?;
    let envelope: AgentDistillEnvelope = serde_json::from_str(&json)
        .map_err(|e| distill_error(format!("local-agent distill JSON parse failed: {e}")))?;
    if envelope.candidates.is_empty() {
        return Err(distill_error(
            "local-agent distill returned no candidates".to_owned(),
        ));
    }
    Ok(envelope.candidates)
}

async fn write_agent_candidates(
    db: &SqlitePool,
    source_repo: &RepoScope,
    seeds: &[DistillSeed],
    candidates: Vec<AgentDistillCandidate>,
    progress: &mut LocalCandidateProgress,
) -> Result<(), LocalAgentDistillError> {
    let mut seen_candidate_signatures = HashSet::new();
    for candidate in candidates {
        if local_candidate_budget_reached(progress) {
            progress.capped = true;
            break;
        }
        let Some(input) = input_from_agent_candidate(&candidate, seeds) else {
            progress.comments_skipped += 1;
            continue;
        };
        match difflore_core::skills::is_rejected_signature(db, &input).await {
            Ok(true) => {
                progress.candidates_suppressed_rejected += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                return Err(distill_error(format!(
                    "failed to check rejection tombstone: {e}"
                )));
            }
        }

        let signature = local_candidate_dedupe_signature(&input);
        if seen_candidate_signatures.contains(&signature) {
            progress.candidates_duplicate_in_run += 1;
            continue;
        }
        seen_candidate_signatures.insert(signature);

        match difflore_core::skills::remember_as_candidate_with_confidence_for_repo(
            db,
            input,
            LOCAL_AGENT_CANDIDATE_CONFIDENCE,
            source_repo,
        )
        .await
        {
            Ok(outcome) => {
                if outcome.deduped {
                    if outcome.matched_existing_active {
                        progress.candidates_matched_active += 1;
                    } else {
                        progress.candidates_deduped += 1;
                    }
                } else {
                    progress.candidates_pending += 1;
                    progress.candidates_created += 1;
                }
            }
            Err(e) => return Err(distill_error(format!("failed to create local memory: {e}"))),
        }
    }
    Ok(())
}

fn input_from_agent_candidate(
    candidate: &AgentDistillCandidate,
    seeds: &[DistillSeed],
) -> Option<RememberRuleInput> {
    let seed = candidate
        .source_index
        .and_then(|idx| seeds.iter().find(|seed| seed.index == idx))
        .or_else(|| seeds.first())?;
    let title = candidate
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(seed.input.title.as_str());
    let body = candidate
        .body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(seed.input.body.as_str());
    let file_patterns = sanitized_file_patterns(&candidate.file_patterns)
        .or_else(|| seed.input.file_patterns.clone());

    Some(RememberRuleInput {
        title: difflore_core::observability::privacy::redact_secrets(&truncate_chars(title, 180)),
        body: difflore_core::observability::privacy::redact_secrets(&body_with_source_evidence(
            body,
            &seed.input.body,
        )),
        file_patterns,
        bad_code: None,
        good_code: None,
        severity: Some("medium".to_owned()),
        kind: None,
        category: None,
        origin: Some("pr_review".to_owned()),
        captured_by_client: Some("import-reviews:local-agent".to_owned()),
    })
}

fn sanitized_file_patterns(patterns: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() || !seen.insert(pattern.to_owned()) {
            continue;
        }
        out.push(pattern.to_owned());
        if out.len() >= difflore_core::skills::REMEMBER_FILE_PATTERN_LIMIT {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn body_with_source_evidence(body: &str, seed_body: &str) -> String {
    if body.contains("Source evidence:") {
        return truncate_chars(body, difflore_core::skills::REMEMBER_BODY_CHAR_LIMIT);
    }
    let source_evidence = seed_body
        .split("\n\nSource evidence:")
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(source_evidence) = source_evidence else {
        return truncate_chars(body, difflore_core::skills::REMEMBER_BODY_CHAR_LIMIT);
    };
    truncate_chars(
        &format!("{body}\n\nSource evidence:\n{source_evidence}"),
        difflore_core::skills::REMEMBER_BODY_CHAR_LIMIT,
    )
}

fn extract_json_payload(stdout: &str) -> Option<String> {
    let trimmed = strip_json_fence(stdout.trim());
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_owned());
    }
    let start = trimmed.find('{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    for (offset, ch) in trimmed[start..].char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[start..=start + offset].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_json_fence(s: &str) -> &str {
    let stripped = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .map_or(s, str::trim_start);
    stripped.strip_suffix("```").map_or(stripped, str::trim_end)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

const fn distill_error(message: String) -> LocalAgentDistillError {
    LocalAgentDistillError { message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(index: usize) -> DistillSeed {
        DistillSeed {
            index,
            input: RememberRuleInput {
                title: "Review: validate API responses".to_owned(),
                body: "Rule:\nValidate API responses before deserializing.\n\nSource evidence:\nSource: acme/api#42\nComment: https://example.test/c\nFile: src/api/client.ts\nReviewer said:\nPlease validate the response first.".to_owned(),
                file_patterns: Some(vec!["src/api/**/*.ts".to_owned()]),
                bad_code: None,
                good_code: None,
                severity: Some("medium".to_owned()),
                kind: None,
                category: None,
                origin: Some("pr_review".to_owned()),
                captured_by_client: Some("import-reviews".to_owned()),
            },
        }
    }

    #[test]
    fn parse_distill_candidates_accepts_fenced_json_object() {
        let parsed = parse_distill_candidates(
            "```json\n{\"candidates\":[{\"source_index\":1,\"title\":\"T\",\"body\":\"B\",\"file_patterns\":[\"**/*.rs\"]}]}\n```",
        )
        .expect("parse");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].source_index, Some(1));
        assert_eq!(parsed[0].title.as_deref(), Some("T"));
    }

    #[test]
    fn parse_distill_candidates_rejects_empty_result_for_heuristic_fallback() {
        let err =
            parse_distill_candidates("{\"candidates\":[]}").expect_err("empty result falls back");

        assert!(
            err.to_string().contains("returned no candidates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn input_from_agent_candidate_keeps_pending_import_origin_and_source_evidence() {
        let seeds = vec![seed(1)];
        let input = input_from_agent_candidate(
            &AgentDistillCandidate {
                source_index: Some(1),
                title: Some("Validate API responses".to_owned()),
                body: Some("Rule:\nValidate API responses before deserializing.".to_owned()),
                file_patterns: vec!["src/**/*.ts".to_owned()],
            },
            &seeds,
        )
        .expect("input");

        assert_eq!(input.origin.as_deref(), Some("pr_review"));
        assert_eq!(
            input.captured_by_client.as_deref(),
            Some("import-reviews:local-agent")
        );
        assert!(input.body.contains("Source evidence:"));
        assert_eq!(input.file_patterns, Some(vec!["src/**/*.ts".to_owned()]));
    }

    #[test]
    fn local_agent_budget_caps_attempt_and_total_windows() {
        assert_eq!(
            local_agent_budget(Duration::ZERO),
            Some(LOCAL_AGENT_ATTEMPT_TIMEOUT)
        );
        assert_eq!(
            local_agent_budget(Duration::from_secs(119)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(local_agent_budget(LOCAL_AGENT_TOTAL_TIMEOUT), None);
    }
}
