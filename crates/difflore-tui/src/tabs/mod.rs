// Top-level tab set for the TUI, named in the product vocabulary
// (Memory / Fixes / Cloud / Setup); rule/skill stay domain-layer words.
//
// The TUI is a read-only inspector and conversion bridge; editorial actions
// (edits, publishing, extraction review) open difflore.dev deep links.
//
//   1. Memory — browse the memory corpus; deep-link to cloud for edits
//   2. Fixes  — review activity, fire heatmaps, and daily impact
//   3. Cloud  — collaboration awareness and cloud onboarding
//   4. Setup  — config, diagnostics, and setup guidance

pub mod cloud;
pub mod fixes;
pub mod memory;
pub mod setup;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tab {
    #[default]
    Memory,
    Fixes,
    Cloud,
    Setup,
}

impl Tab {
    pub(crate) const ALL: [Self; 4] = [Self::Memory, Self::Fixes, Self::Cloud, Self::Setup];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Memory => "Memory",
            Self::Fixes => "Fixes",
            Self::Cloud => "Cloud",
            Self::Setup => "Setup",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Memory => 0,
            Self::Fixes => 1,
            Self::Cloud => 2,
            Self::Setup => 3,
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
            1 => Some(Self::Memory),
            2 => Some(Self::Fixes),
            3 => Some(Self::Cloud),
            4 => Some(Self::Setup),
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
        assert_eq!(Tab::Memory.prev(), Tab::Setup);
        assert_eq!(Tab::Setup.next(), Tab::Memory);
    }

    #[test]
    fn index_matches_tab_order() {
        for (index, tab) in Tab::ALL.iter().copied().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }
}
