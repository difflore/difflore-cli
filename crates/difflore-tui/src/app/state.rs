use difflore_core::domain::models::SkillRecord;

use crate::tabs::rules::{RepoScope, ordered_filtered_rules};

use super::{App, RulesOriginFilter, RulesRepoFilter};

impl App {
    /// Snap the list selection to the first visible row when the filter
    /// changes, so the cursor doesn't land on a stale index showing nothing.
    pub(super) fn reset_selection_after_filter_change(&mut self) {
        if self.filtered_rules_count() == 0 {
            self.state.rules_list_state.select(None);
        } else {
            self.state.rules_list_state.select(Some(0));
        }
    }

    /// `RepoScope` for the active repo filter. Must mirror the scope `render`
    /// passes into `tabs::rules::render` so the visible list and the
    /// selection-derived state share identical inputs.
    fn rules_scope(&self) -> RepoScope<'_> {
        RepoScope {
            source_repos: &self.state.rules_source_repos,
            current_repo: self.state.current_repo.as_deref(),
            filter: self.state.rules_repo_filter,
            filter_label: self.state.rules_repo_filter.label(),
        }
    }

    /// Sorted + filtered rule slice shared with the rendered list (see
    /// `tabs::rules::ordered_filtered_rules`). The cursor indexes into this
    /// order, so `selected_rule` and the cursor clamp must too.
    fn ordered_filtered_rules(&self) -> Vec<&SkillRecord> {
        ordered_filtered_rules(
            &self.state.rules,
            &self.state.rules_origin_filter,
            &self.state.rules_search,
            &self.rules_scope(),
        )
    }

    /// Currently selected rule, after the active origin × repo × search filter
    /// and the list's sort order. Deriving it from the sorted slice keeps
    /// `e/p/s` acting on the highlighted row.
    pub(super) fn selected_rule(&self) -> Option<&SkillRecord> {
        let idx = self.state.rules_list_state.selected()?;
        self.ordered_filtered_rules().into_iter().nth(idx)
    }

    pub(super) fn filtered_rules_count(&self) -> usize {
        self.ordered_filtered_rules().len()
    }

    pub(super) const fn cycle_repo_filter(&mut self) {
        // ThisRepo only makes sense when we know the current repo; otherwise
        // the cycle skips it so the user never lands on an always-empty state.
        let has_repo = self.state.current_repo.is_some();
        self.state.rules_repo_filter = match (self.state.rules_repo_filter, has_repo) {
            (RulesRepoFilter::ThisRepo, _) | (RulesRepoFilter::Global, false) => {
                RulesRepoFilter::All
            }
            (RulesRepoFilter::All, _) => RulesRepoFilter::Global,
            (RulesRepoFilter::Global, true) => RulesRepoFilter::ThisRepo,
        };
    }

    pub(super) fn cycle_origin_filter(&mut self) {
        self.state.rules_origin_filter = match &self.state.rules_origin_filter {
            RulesOriginFilter::CloudMemory => RulesOriginFilter::All,
            RulesOriginFilter::All => RulesOriginFilter::Origin("pr_review".to_owned()),
            RulesOriginFilter::Origin(origin) if origin == "pr_review" => {
                RulesOriginFilter::Origin("extracted".to_owned())
            }
            RulesOriginFilter::Origin(origin) if origin == "extracted" => {
                RulesOriginFilter::Origin("cloud".to_owned())
            }
            RulesOriginFilter::Origin(origin) if origin == "cloud" => {
                RulesOriginFilter::Origin("manual".to_owned())
            }
            RulesOriginFilter::Origin(origin) if origin == "manual" => {
                RulesOriginFilter::Origin("conversation".to_owned())
            }
            RulesOriginFilter::Origin(_) => RulesOriginFilter::CloudMemory,
        };
    }
}
