use std::collections::BTreeMap;

use difflore_core::models::SkillRecord;

use super::{RepoScope, RulesRepoFilter};

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

pub(super) fn origin_distribution(rules: &[&SkillRecord]) -> Vec<(String, usize)> {
    let mut counts = BTreeMap::new();
    for rule in rules {
        *counts.entry(rule.origin.clone()).or_insert(0usize) += 1;
    }

    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|(left, _), (right, _)| {
        difflore_core::origins::distribution_sort_key(left)
            .cmp(&difflore_core::origins::distribution_sort_key(right))
            .then_with(|| left.cmp(right))
    });
    counts
}
