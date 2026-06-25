use std::io::{self, IsTerminal};

use super::{FixAgentMode, FixArgs};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FixOutputMode {
    Handoff,
    Structured,
    Preview,
    Ci,
    Yes,
    Pipe,
    Interactive,
}

impl FixOutputMode {
    pub(super) fn pick(args: &FixArgs, structured_output: bool) -> Self {
        if args.agent == FixAgentMode::Handoff {
            Self::Handoff
        } else if args.yes {
            Self::Yes
        } else if structured_output {
            Self::Structured
        } else if args.preview {
            Self::Preview
        } else if args.ci {
            Self::Ci
        } else if io::stdout().is_terminal() {
            Self::Interactive
        } else {
            Self::Pipe
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_mode_wins_over_provider_apply_modes() {
        let args = FixArgs {
            yes: true,
            preview: false,
            read_only: false,
            ci: false,
            strict: false,
            diff_scope: None,
            pr: None,
            repo: None,
            base: None,
            work_branch: None,
            no_checkout: false,
            allow_dirty: false,
            no_upload_acceptance: false,
            explain_rules: false,
            report: None,
            json: false,
            path: None,
            agent: FixAgentMode::Handoff,
        };

        assert_eq!(FixOutputMode::pick(&args, false), FixOutputMode::Handoff);
    }

    #[test]
    fn yes_mode_wins_over_json_structured_output() {
        let args = FixArgs {
            yes: true,
            preview: false,
            read_only: false,
            ci: false,
            strict: false,
            diff_scope: None,
            pr: None,
            repo: None,
            base: None,
            work_branch: None,
            no_checkout: false,
            allow_dirty: false,
            no_upload_acceptance: false,
            explain_rules: false,
            report: None,
            json: true,
            path: None,
            agent: FixAgentMode::Provider,
        };

        assert_eq!(FixOutputMode::pick(&args, true), FixOutputMode::Yes);
    }
}
