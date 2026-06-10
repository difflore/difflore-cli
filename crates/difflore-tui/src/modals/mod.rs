//! TUI modal queue and variants.
//!
//! `ModalStack` keeps one current modal plus a priority-sorted queue.
//! `try_show` is idempotent by modal kind, and `dismiss` advances to
//! the next pending modal. Key → action routing and render fan-out live
//! in [`dispatch`]; each modal file owns its copy and keymap.

pub mod cross_machine;
pub(crate) mod dispatch;
pub mod fix_runs_low;
pub mod onboarding;
pub mod teammate_caught;

use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    CrossMachine {
        other_host: String,
    },
    TeammateCaught {
        rule: String,
        teammate: String,
        fired_at: String,
    },
    FixRunsLow {
        used: u32,
        quota: u32,
    },
    /// `step` is 1..=5.
    Onboarding {
        step: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModalKind {
    CrossMachine,
    TeammateCaught,
    FixRunsLow,
    Onboarding,
}

impl Modal {
    pub const fn kind(&self) -> ModalKind {
        match self {
            Self::CrossMachine { .. } => ModalKind::CrossMachine,
            Self::TeammateCaught { .. } => ModalKind::TeammateCaught,
            Self::FixRunsLow { .. } => ModalKind::FixRunsLow,
            Self::Onboarding { .. } => ModalKind::Onboarding,
        }
    }
}

#[derive(Default)]
pub struct ModalStack {
    queue: VecDeque<Modal>,
    current: Option<Modal>,
}

impl ModalStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a modal, dropping it if the same `kind` is already queued or
    /// current (idempotent under repeat triggers). The queue is re-sorted by
    /// `priority` so the highest-priority modal is popped first.
    pub fn try_show(&mut self, modal: Modal) {
        let k = modal.kind();
        if self.current.as_ref().map(Modal::kind) == Some(k)
            || self.queue.iter().any(|m| m.kind() == k)
        {
            return;
        }
        self.queue.push_back(modal);
        self.queue
            .make_contiguous()
            .sort_by_key(|m| std::cmp::Reverse(Self::priority(m)));
        if self.current.is_none() {
            self.current = self.queue.pop_front();
        }
    }

    pub const fn current(&self) -> Option<&Modal> {
        self.current.as_ref()
    }

    /// Dismiss the current modal, advance to the next.
    pub fn dismiss(&mut self) -> Option<Modal> {
        let prev = self.current.take();
        self.current = self.queue.pop_front();
        prev
    }

    pub fn is_empty(&self) -> bool {
        self.current.is_none() && self.queue.is_empty()
    }

    /// Onboarding > `TeammateCaught` > `FixRunsLow` > `CrossMachine`.
    pub const fn priority(modal: &Modal) -> u8 {
        match modal.kind() {
            ModalKind::Onboarding => 4,
            ModalKind::TeammateCaught => 3,
            ModalKind::FixRunsLow => 2,
            ModalKind::CrossMachine => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_push_becomes_current() {
        let mut s = ModalStack::new();
        s.try_show(Modal::CrossMachine {
            other_host: "host-a".into(),
        });
        assert_eq!(s.current().map(Modal::kind), Some(ModalKind::CrossMachine));
    }

    #[test]
    fn duplicate_kinds_are_dropped() {
        let mut s = ModalStack::new();
        s.try_show(Modal::FixRunsLow {
            used: 240,
            quota: 300,
        });
        s.try_show(Modal::FixRunsLow {
            used: 250,
            quota: 300,
        });
        // Dismiss current, queue should now be empty (the duplicate
        // was dropped by `try_show`'s kind check).
        let _ = s.dismiss();
        assert!(s.current().is_none());
    }

    #[test]
    fn priority_sorts_queue_after_push() {
        let mut s = ModalStack::new();
        s.try_show(Modal::CrossMachine {
            other_host: "a".into(),
        });
        // CrossMachine becomes current. Push lower-priority filler
        // that has no kind collision and a higher-priority modal.
        s.try_show(Modal::FixRunsLow {
            used: 1,
            quota: 100,
        });
        s.try_show(Modal::TeammateCaught {
            rule: "r".into(),
            teammate: "t".into(),
            fired_at: "now".into(),
        });
        // Dismiss current → pop highest-priority queued (Teammate).
        s.dismiss();
        assert_eq!(
            s.current().map(Modal::kind),
            Some(ModalKind::TeammateCaught)
        );
        // Then FixRunsLow.
        s.dismiss();
        assert_eq!(s.current().map(Modal::kind), Some(ModalKind::FixRunsLow));
    }

    #[test]
    fn onboarding_outranks_everything_else() {
        let mut s = ModalStack::new();
        s.try_show(Modal::CrossMachine {
            other_host: "a".into(),
        });
        s.try_show(Modal::Onboarding { step: 1 });
        s.dismiss();
        assert_eq!(s.current().map(Modal::kind), Some(ModalKind::Onboarding));
    }
}
