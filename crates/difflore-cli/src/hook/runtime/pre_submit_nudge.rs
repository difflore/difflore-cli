use crate::hook::adapters::types::HookResult;

const PRE_SUBMIT_NUDGE: &str = "DiffLore pre-submit review: before committing, pushing, or opening a PR, run `difflore review --diff all`; fix real findings, then run it again. Use the full flow in `difflore://skills/pre-submit-review`. Do not commit, push, open a PR, or apply broad rewrites unless the user explicitly asks.";

const ENGLISH_POSITIVE_PHRASES: &[&str] = &[
    "pre-submit",
    "pre submit",
    "before commit",
    "before committing",
    "before push",
    "before pushing",
    "before pr",
    "before opening a pr",
    "before pull request",
    "before submitting",
    "ready to commit",
    "ready to push",
    "open a pr",
    "create a pr",
    "submit code",
    "final code review",
    "final review before",
    "ship this",
    "ship it",
];

const ENGLISH_NEGATIVE_PHRASES: &[&str] = &[
    "don't commit",
    "dont commit",
    "do not commit",
    "don't push",
    "dont push",
    "do not push",
    "don't open a pr",
    "do not open a pr",
];

const CHINESE_POSITIVE_PHRASES: &[&str] = &[
    "提交前",
    "提交代码前",
    "准备提交",
    "帮我提交",
    "推送前",
    "推送代码",
    "发pr",
    "发 pr",
    "提pr",
    "提 pr",
    "开pr",
    "开 pr",
    "pr前",
    "pr 前",
    "合并前",
    "最终检查",
    "最后检查一下",
    "发布前检查",
];

const CHINESE_NEGATIVE_PHRASES: &[&str] = &[
    "不要提交",
    "先别提交",
    "不用提交",
    "不要推送",
    "先别推送",
    "不用推送",
];

pub(super) fn nudge_for_prompt(prompt: &str) -> Option<HookResult> {
    has_pre_submit_intent(prompt).then(|| HookResult::with_context(PRE_SUBMIT_NUDGE))
}

fn has_pre_submit_intent(prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return false;
    }

    let lower = prompt.to_ascii_lowercase();
    if contains_any(&lower, ENGLISH_NEGATIVE_PHRASES)
        || contains_any(prompt, CHINESE_NEGATIVE_PHRASES)
    {
        return false;
    }

    contains_any(&lower, ENGLISH_POSITIVE_PHRASES) || contains_any(prompt, CHINESE_POSITIVE_PHRASES)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::adapters::PlatformAdapter;

    #[test]
    fn detects_english_pre_submit_intent() {
        let result = nudge_for_prompt("Before committing, can you do the final code review?")
            .expect("pre-submit wording should nudge");

        let ctx = result.additional_context.expect("nudge context");
        assert!(ctx.contains("difflore review --diff all"));
        assert!(ctx.contains("difflore://skills/pre-submit-review"));
        assert!(ctx.contains("Do not commit"));
    }

    #[test]
    fn detects_chinese_pre_submit_intent() {
        assert!(nudge_for_prompt("提交前帮我最后检查一下").is_some());
        assert!(nudge_for_prompt("准备提交到远程了，先检查").is_some());
    }

    #[test]
    fn broad_git_or_negated_mentions_stay_noop() {
        assert!(nudge_for_prompt("Can you explain git commit message style?").is_none());
        assert!(nudge_for_prompt("Do not commit yet; just inspect the status.").is_none());
        assert!(nudge_for_prompt("先别提交，看看 diff。").is_none());
    }

    #[test]
    fn claude_output_surfaces_pre_submit_nudge_as_context() {
        let mut result =
            nudge_for_prompt("ready to push; please do a final review").expect("should nudge");
        result.event_name = Some("UserPromptSubmit".to_owned());

        let adapter = crate::hook::adapters::claude_code::ClaudeCodeAdapter;
        let out = adapter.format_output(result);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");

        assert_eq!(value["continue"], true);
        assert_eq!(
            value["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            value["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .expect("additional context")
                .contains("pre-submit-review")
        );
    }
}
