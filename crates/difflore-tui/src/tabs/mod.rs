// Top-level tab set for the TUI.
//
// The TUI is a read-only inspector and conversion bridge; editorial actions
// (edits, publishing, extraction review) open difflore.dev deep links.
//
//   1. Rules     — browse the rule corpus; deep-link to cloud for edits
//   2. Activity  — review activity, fire heatmaps, and daily impact
//   3. Team      — collaboration awareness and cloud onboarding
//   4. Settings  — config, diagnostics, and setup guidance

pub mod activity;
pub mod rules;
pub mod settings;
pub mod team;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tab {
    #[default]
    Rules,
    Activity,
    Team,
    Settings,
}

impl Tab {
    pub(crate) const ALL: [Self; 4] = [Self::Rules, Self::Activity, Self::Team, Self::Settings];

    pub(crate) const fn title(self) -> &'static str {
        // Display labels differ from the enum variant names; the variants stay
        // as-is to avoid renaming modules and call sites.
        match self {
            Self::Rules => "Memory",
            Self::Activity => "Fixes",
            Self::Team => "Cloud",
            Self::Settings => "Setup",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Rules => 0,
            Self::Activity => 1,
            Self::Team => 2,
            Self::Settings => 3,
        }
    }

    pub(crate) const fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub(crate) const fn prev(self) -> Self {
        let len = Self::ALL.len();
        Self::ALL[(self.index() + len - 1) % len]
    }

    /// Map a single keyboard digit (`1`..=`4`) to a tab.
    pub(crate) const fn from_digit(d: u8) -> Option<Self> {
        match d {
            1 => Some(Self::Rules),
            2 => Some(Self::Activity),
            3 => Some(Self::Team),
            4 => Some(Self::Settings),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_digit_round_trips() {
        for (d, t) in (1u8..=4).zip(Tab::ALL) {
            assert_eq!(Tab::from_digit(d), Some(t));
        }
        assert_eq!(Tab::from_digit(0), None);
        assert_eq!(Tab::from_digit(5), None);
    }

    #[test]
    fn next_prev_wrap() {
        assert_eq!(Tab::Rules.prev(), Tab::Settings);
        assert_eq!(Tab::Settings.next(), Tab::Rules);
    }

    #[test]
    fn index_matches_tab_order() {
        for (index, tab) in Tab::ALL.iter().copied().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }
}
