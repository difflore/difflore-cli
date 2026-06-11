//! Filter state for the Memory tab: origin / repo / search / focus enums
//! plus the origin-count selectors that drive default filters and the
//! header strip.

use std::collections::BTreeMap;

use difflore_core::domain::models::SkillRecord;

use super::RepoScope;

/// Three-way scope toggle for the Memory tab. Default is `ThisRepo` when
/// the user launched the TUI inside a known GitHub repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulesRepoFilter {
    ThisRepo,
    All,
    Global,
}

impl RulesRepoFilter {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ThisRepo => "this repo",
            Self::All => "all repos",
            Self::Global => "global",
        }
    }
}

/// Memory view selector. `CloudMemory` is the product-facing set and matches
/// the cloud Memory page: accepted/synced cloud rules plus extracted review
/// memories. `All` keeps local raw imports visible without inflating the
/// default memory count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RulesOriginFilter {
    #[default]
    CloudMemory,
    All,
    Origin(String),
}

impl RulesOriginFilter {
    pub(crate) fn includes_origin(&self, origin: &str) -> bool {
        match self {
            Self::CloudMemory => is_cloud_memory_origin(origin),
            Self::All => true,
            Self::Origin(want) => origin == want,
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::CloudMemory => "cloud memory".to_owned(),
            Self::All => "all local".to_owned(),
            Self::Origin(origin) => origin.clone(),
        }
    }
}

/// Three-state machine for the Memory tab `/` search. `Off` / `Editing(q)` /
/// `Filtering(q)` matches the model fzf / lf / less use; `Esc` always
/// returns to `Off`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RulesSearch {
    #[default]
    Off,
    Editing(String),
    Filtering(String),
}

impl RulesSearch {
    pub(crate) const fn query(&self) -> Option<&str> {
        match self {
            Self::Off => None,
            Self::Editing(q) | Self::Filtering(q) => Some(q.as_str()),
        }
    }

    pub(crate) const fn is_editing(&self) -> bool {
        matches!(self, Self::Editing(_))
    }
}

/// Which pane in the Memory tab currently has keyboard focus. Switched with
/// `h` / `l`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RulesFocus {
    #[default]
    List,
    Detail,
}

impl RepoScope<'_> {
    pub(super) fn includes(&self, rule: &SkillRecord) -> bool {
        let repo = self.source_repos.get(&rule.id).and_then(|v| v.as_deref());
        match self.filter {
            RulesRepoFilter::All => true,
            RulesRepoFilter::ThisRepo => match (self.current_repo, repo) {
                (Some(want), Some(got)) => got == want,
                _ => false,
            },
            RulesRepoFilter::Global => repo.is_none(),
        }
    }
}

pub(crate) fn is_cloud_memory_origin(origin: &str) -> bool {
    matches!(origin, "cloud" | "extracted")
}

pub(crate) fn cloud_memory_rule_count(rules: &[SkillRecord]) -> usize {
    rules
        .iter()
        .filter(|rule| is_cloud_memory_origin(&rule.origin))
        .count()
}

pub(crate) fn raw_local_rule_count(rules: &[SkillRecord]) -> usize {
    rules
        .iter()
        .filter(|rule| !is_cloud_memory_origin(&rule.origin))
        .count()
}

pub(crate) fn primary_memory_rule_count(rules: &[SkillRecord]) -> usize {
    let cloud_memory = cloud_memory_rule_count(rules);
    if cloud_memory > 0 {
        cloud_memory
    } else {
        rules.len()
    }
}

pub(crate) fn default_origin_filter(rules: &[SkillRecord]) -> RulesOriginFilter {
    if cloud_memory_rule_count(rules) > 0 {
        RulesOriginFilter::CloudMemory
    } else {
        RulesOriginFilter::All
    }
}

pub(super) fn origin_distribution(rules: &[&SkillRecord]) -> Vec<(String, usize)> {
    let mut counts = BTreeMap::new();
    for rule in rules {
        *counts.entry(rule.origin.clone()).or_insert(0usize) += 1;
    }

    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|(left, _), (right, _)| {
        difflore_core::domain::origins::distribution_sort_key(left)
            .cmp(&difflore_core::domain::origins::distribution_sort_key(
                right,
            ))
            .then_with(|| left.cmp(right))
    });
    counts
}

/// Shared test fixture: a minimal `SkillRecord` with the given origin.
/// Crate-visible so sibling test modules (plan state, app) reuse it.
#[cfg(test)]
pub(crate) fn rule_with_origin(origin: &str) -> SkillRecord {
    SkillRecord {
        id: format!("{origin}-id"),
        name: format!("{origin}-rule"),
        source: "local".into(),
        directory: "/tmp/rule".into(),
        version: "1.0.0".into(),
        description: "sample".into(),
        r#type: "workflow".into(),
        engines: Vec::new(),
        tags: Vec::new(),
        trigger: None,
        check_prompt: None,
        repo_owner: None,
        repo_name: None,
        repo_branch: None,
        readme_url: None,
        enabled_for_codex: true,
        enabled_for_claude: false,
        enabled_for_gemini: false,
        enabled_for_cursor: false,
        installed_at: "2026-01-01".into(),
        updated_at: "2026-01-01".into(),
        enforcement: None,
        origin: origin.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_memory_count_excludes_raw_local_imports() {
        let rules = vec![
            rule_with_origin("cloud"),
            rule_with_origin("extracted"),
            rule_with_origin("pr_review"),
            rule_with_origin("manual"),
            rule_with_origin("conversation"),
        ];

        assert_eq!(cloud_memory_rule_count(&rules), 2);
        assert_eq!(raw_local_rule_count(&rules), 3);
        assert_eq!(
            default_origin_filter(&rules),
            RulesOriginFilter::CloudMemory
        );
    }
}
