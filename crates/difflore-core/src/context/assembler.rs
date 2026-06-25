use super::retrieval::ScoredRuleChunk;
use super::rule_source::RuleExample;
use super::types::PastVerdict;

pub const RULE_TOKEN_BUDGET: usize = 1500;
pub const SOFT_PREFERENCE_TOKEN_BUDGET: usize = 240;

/// Per-call token budget for assembled rule context. Defaults to a compile-time
/// constant; callers may override based on per-project settings.
#[derive(Debug, Clone, Copy)]
pub struct TokenBudgets {
    pub rule: usize,
    pub soft_preference: usize,
}

impl Default for TokenBudgets {
    fn default() -> Self {
        Self {
            rule: RULE_TOKEN_BUDGET,
            soft_preference: SOFT_PREFERENCE_TOKEN_BUDGET,
        }
    }
}

impl TokenBudgets {
    /// Build from optional settings overrides. Non-positive values fall back
    /// to the compile-time defaults.
    pub fn from_overrides(rule: Option<i32>) -> Self {
        let rule = rule
            .filter(|v| *v > 0)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(RULE_TOKEN_BUDGET);
        Self {
            rule,
            soft_preference: SOFT_PREFERENCE_TOKEN_BUDGET,
        }
    }
}

const fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[derive(Debug, Clone)]
pub struct ContextSection {
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub soft_preference_sections: Vec<ContextSection>,
    pub rule_sections: Vec<ContextSection>,
    pub soft_preference_count: usize,
    pub rule_count: usize,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone)]
pub struct SoftPreferenceContext {
    pub title: String,
    pub body: String,
}

/// Format a rule with its few-shot examples for prompt injection.
fn format_rule_with_examples(rule_content: &str, examples: Option<&Vec<RuleExample>>) -> String {
    let mut text = rule_content.to_owned();

    if let Some(examples) = examples
        && !examples.is_empty()
    {
        text.push_str("\n\n### Examples\n");
        for (i, ex) in examples.iter().enumerate() {
            if let Some(desc) = &ex.description {
                text.push_str(&format!("\n**Example {}**: {}\n", i + 1, desc));
            } else {
                text.push_str(&format!("\n**Example {}**:\n", i + 1));
            }
            text.push_str(&format!(
                "\n❌ Bad:\n```\n{}\n```\n\n✅ Good:\n```\n{}\n```\n",
                ex.bad_code, ex.good_code
            ));
        }
    }

    text
}

pub fn assemble(
    rule_chunks: &[ScoredRuleChunk],
    query: &str,
    task_intent: &str,
) -> AssembledContext {
    assemble_with_examples_and_budgets(
        rule_chunks,
        query,
        task_intent,
        None,
        TokenBudgets::default(),
    )
}

#[allow(clippy::implicit_hasher)] // reason: stable public API; `HashMap<K,V>` (default hasher) is what every caller passes.
pub fn assemble_with_examples(
    rule_chunks: &[ScoredRuleChunk],
    query: &str,
    task_intent: &str,
    examples_map: Option<&std::collections::HashMap<String, Vec<RuleExample>>>,
) -> AssembledContext {
    assemble_with_examples_and_budgets(
        rule_chunks,
        query,
        task_intent,
        examples_map,
        TokenBudgets::default(),
    )
}

#[allow(clippy::implicit_hasher)] // reason: stable public API; `HashMap<K,V>` (default hasher) is what every caller passes.
pub fn assemble_with_examples_and_budgets(
    rule_chunks: &[ScoredRuleChunk],
    query: &str,
    task_intent: &str,
    examples_map: Option<&std::collections::HashMap<String, Vec<RuleExample>>>,
    budgets: TokenBudgets,
) -> AssembledContext {
    assemble_with_examples_budgets_and_soft_preferences(
        rule_chunks,
        query,
        task_intent,
        examples_map,
        budgets,
        &[],
    )
}

#[allow(clippy::implicit_hasher)] // reason: stable public API; `HashMap<K,V>` (default hasher) is what every caller passes.
pub fn assemble_with_examples_budgets_and_soft_preferences(
    rule_chunks: &[ScoredRuleChunk],
    query: &str,
    task_intent: &str,
    examples_map: Option<&std::collections::HashMap<String, Vec<RuleExample>>>,
    budgets: TokenBudgets,
    soft_preferences: &[SoftPreferenceContext],
) -> AssembledContext {
    let mut soft_preference_sections = Vec::new();
    let mut soft_preference_tokens = 0;
    let mut rule_sections = Vec::new();
    let mut rule_tokens = 0;

    for preference in soft_preferences {
        let section_text = format_soft_preference(preference);
        let tokens = estimate_tokens(&section_text);
        if soft_preference_tokens + tokens > budgets.soft_preference {
            continue;
        }
        soft_preference_tokens += tokens;
        soft_preference_sections.push(ContextSection {
            content: section_text,
        });
    }

    for scored in rule_chunks {
        let examples = examples_map.and_then(|m| m.get(&scored.skill_id));
        let section_text = format_rule_with_examples(&scored.content, examples);
        let tokens = estimate_tokens(&section_text);
        if rule_tokens + tokens > budgets.rule {
            continue;
        }
        rule_tokens += tokens;
        rule_sections.push(ContextSection {
            content: section_text,
        });
    }

    let _query = query;
    let _task_intent = task_intent;

    AssembledContext {
        soft_preference_count: soft_preference_sections.len(),
        rule_count: rule_sections.len(),
        soft_preference_sections,
        rule_sections,
        estimated_tokens: soft_preference_tokens + rule_tokens,
    }
}

fn format_soft_preference(preference: &SoftPreferenceContext) -> String {
    format!(
        "Preference: {}\n{}",
        preference.title.trim(),
        preference.body.trim()
    )
}

/// Render a past-verdict recall block for injection into the review
/// prompt. Review memory places this at the front of the dynamic suffix so the
/// LLM reads prior decisions before the current diff.
#[derive(Debug, Clone)]
pub struct PastVerdictSection {
    pub entries: Vec<PastVerdict>,
}

impl PastVerdictSection {
    pub const fn new(entries: Vec<PastVerdict>) -> Self {
        Self { entries }
    }

    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the section as a markdown snippet. Returns an empty string
    /// when there are no entries, so call sites can unconditionally splice
    /// the result into a prompt without worrying about stray headers.
    pub fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut s = String::new();
        s.push_str("## Past verdicts on similar code\n\n");
        s.push_str("The following similar code pieces were previously reviewed:\n\n");
        for (i, v) in self.entries.iter().enumerate() {
            s.push_str(&format!(
                "{}. [{}, similarity {:.2}] {}\n",
                i + 1,
                v.status,
                v.similarity,
                v.code_snippet,
            ));
            s.push_str(&format!("   Issue: {}\n", v.issue_text));
            if let Some(reason) = v.reason.as_ref()
                && !reason.is_empty()
            {
                s.push_str(&format!("   Reason: {reason}\n"));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::retrieval::ScoredRuleChunk;
    use crate::context::types::PastVerdict;

    fn make_rule_chunk(skill_id: &str, content: &str) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: skill_id.to_owned(),
            content: content.to_owned(),
            score: 1.0,
            confidence: 0.8,
        }
    }

    #[test]
    fn estimate_tokens_approximates_four_chars_per_token() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("ab"), 1); // (2+3)/4 = 1
        assert_eq!(estimate_tokens("abcdefgh"), 2); // (8+3)/4 = 2
    }

    #[test]
    fn assemble_respects_rule_token_budget() {
        let big_rule = "r".repeat(2000);
        let rules: Vec<ScoredRuleChunk> = (0..10)
            .map(|i| make_rule_chunk(&format!("s{i}"), &big_rule))
            .collect();

        let assembled = assemble(&rules, "q", "i");
        assert!(
            assembled.rule_count < 10,
            "expected rule budget to truncate, got {}",
            assembled.rule_count
        );
    }

    #[test]
    fn token_budgets_from_overrides_uses_defaults_when_invalid() {
        let b = TokenBudgets::from_overrides(None);
        assert_eq!(b.rule, RULE_TOKEN_BUDGET);

        let b = TokenBudgets::from_overrides(Some(-5));
        assert_eq!(b.rule, RULE_TOKEN_BUDGET);
    }

    #[test]
    fn token_budgets_from_overrides_accepts_positive_values() {
        let b = TokenBudgets::from_overrides(Some(50));
        assert_eq!(b.rule, 50);
    }

    #[test]
    fn assemble_with_smaller_budget_truncates_more_aggressively() {
        let big_rule = "r".repeat(2000);
        let rules: Vec<ScoredRuleChunk> = (0..10)
            .map(|i| make_rule_chunk(&format!("s{i}"), &big_rule))
            .collect();

        let small_budget = TokenBudgets {
            rule: 100,
            ..TokenBudgets::default()
        };
        let assembled = assemble_with_examples_and_budgets(&rules, "q", "i", None, small_budget);
        assert!(
            assembled.rule_count <= 1,
            "expected aggressive truncation, got {}",
            assembled.rule_count,
        );
    }

    #[test]
    fn assemble_skips_oversized_rule_and_keeps_later_fitting_rules() {
        let rules = vec![
            make_rule_chunk("small-1", &"a".repeat(20)),
            make_rule_chunk("oversized", &"b".repeat(200)),
            make_rule_chunk("small-2", &"c".repeat(20)),
        ];

        let assembled = assemble_with_examples_and_budgets(
            &rules,
            "q",
            "i",
            None,
            TokenBudgets {
                rule: 10,
                ..TokenBudgets::default()
            },
        );

        assert_eq!(assembled.rule_count, 2);
        assert_eq!(assembled.estimated_tokens, 10);
        assert_eq!(assembled.rule_sections[0].content, "a".repeat(20));
        assert_eq!(assembled.rule_sections[1].content, "c".repeat(20));
    }

    #[test]
    fn soft_preferences_use_independent_budget_before_rules() {
        let rules = vec![make_rule_chunk("rule-1", &"r".repeat(20))];
        let soft_preferences = vec![SoftPreferenceContext {
            title: "Prefer backend-first answers".to_owned(),
            body: "When tradeoffs are unclear, prioritize backend maintainability.".to_owned(),
        }];
        let assembled = assemble_with_examples_budgets_and_soft_preferences(
            &rules,
            "q",
            "i",
            None,
            TokenBudgets {
                rule: 10,
                soft_preference: 80,
            },
            &soft_preferences,
        );

        assert_eq!(assembled.soft_preference_count, 1);
        assert_eq!(assembled.rule_count, 1);
        assert!(
            assembled.soft_preference_sections[0]
                .content
                .contains("Preference: Prefer backend-first answers")
        );
    }

    fn sample_verdict(
        id: &str,
        status: &str,
        snippet: &str,
        issue: &str,
        reason: Option<&str>,
        sim: f32,
    ) -> PastVerdict {
        PastVerdict {
            extraction_id: id.into(),
            code_snippet: snippet.into(),
            issue_text: issue.into(),
            status: status.into(),
            reason: reason.map(Into::into),
            similarity: sim,
            created_at: "2026-04-10T00:00:00Z".into(),
            signature: None,
            source_pr_number: None,
            source_pr_title: None,
            source_pr_url: None,
        }
    }

    #[test]
    fn test_past_verdict_section_empty_renders_empty_string() {
        let section = PastVerdictSection::new(Vec::new());
        assert!(section.is_empty());
        assert_eq!(section.render(), "");
    }

    #[test]
    fn test_past_verdict_section_renders_entries() {
        let section = PastVerdictSection::new(vec![
            sample_verdict(
                "e1",
                "approved",
                "let x = value.unwrap();",
                "unwrap can panic",
                Some("panics on None at runtime"),
                0.874,
            ),
            sample_verdict(
                "e2",
                "rejected",
                "println!(\"debug\");",
                "debug print left in code",
                None,
                0.612,
            ),
        ]);

        let out = section.render();
        // Header + intro
        assert!(out.contains("## Past verdicts on similar code"));
        assert!(out.contains("similar code pieces were previously reviewed"));
        // First entry -- includes status, formatted similarity, snippet, issue, reason
        assert!(out.contains("[approved, similarity 0.87]"));
        assert!(out.contains("let x = value.unwrap();"));
        assert!(out.contains("Issue: unwrap can panic"));
        assert!(out.contains("Reason: panics on None at runtime"));
        // Second entry -- rejected, no reason line
        assert!(out.contains("[rejected, similarity 0.61]"));
        assert!(out.contains("println!(\"debug\");"));
        // Numbered 1. and 2.
        assert!(out.contains("1. "));
        assert!(out.contains("2. "));
    }

    #[test]
    fn assemble_includes_examples_when_provided() {
        let rule = make_rule_chunk("skill1", "Always prefer `?` over unwrap()");
        let mut examples_map = std::collections::HashMap::new();
        examples_map.insert(
            "skill1".to_owned(),
            vec![RuleExample {
                id: "ex1".into(),
                skill_id: "skill1".into(),
                description: Some("unwrap vs ?".into()),
                bad_code: "value.unwrap()".into(),
                good_code: "value?".into(),
                source: "manual".into(),
            }],
        );

        let assembled = assemble_with_examples(&[rule], "q", "i", Some(&examples_map));
        assert_eq!(assembled.rule_count, 1);
        let content = &assembled.rule_sections[0].content;
        assert!(content.contains("Example 1"));
        assert!(content.contains("value.unwrap()"));
        assert!(content.contains("value?"));
    }
}
