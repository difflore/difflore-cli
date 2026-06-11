use crate::hook::adapters::types::HookResult;

const REMEMBER_NUDGE: &str = "DiffLore memory nudge: the user explicitly asked for this to be remembered. Consider turning it into a candidate preference/rule through the existing memory flow; do not silently persist sensitive or one-off data.";

const ENGLISH_POSITIVE_PHRASES: &[&str] = &[
    "remember this",
    "please remember",
    "from now on",
    "next time don't",
    "next time dont",
    "next time do not",
];

const ENGLISH_NEGATIVE_PHRASES: &[&str] = &[
    "don't remember this",
    "dont remember this",
    "do not remember this",
    "can't remember this",
    "cannot remember this",
    "not remember this",
    "no need to remember this",
];

const CHINESE_POSITIVE_PHRASES: &[&str] = &["记住这个", "帮我记住", "以后都这样", "下次不要"];
const CHINESE_NEGATIVE_PHRASES: &[&str] = &["别记住这个", "不要记住这个", "不用记住", "无需记住"];

pub(super) fn nudge_for_prompt(prompt: &str) -> Option<HookResult> {
    has_explicit_remember_intent(prompt).then(|| HookResult::with_context(REMEMBER_NUDGE))
}

fn has_explicit_remember_intent(prompt: &str) -> bool {
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
    fn detects_chinese_explicit_memory_intent() {
        let result = nudge_for_prompt("帮我记住：这个项目里不要自动改 installer manifest。")
            .expect("explicit Chinese remember request should nudge");

        let ctx = result.additional_context.expect("nudge context");
        assert!(ctx.contains("explicitly asked"));
        assert!(ctx.contains("do not silently persist"));
    }

    #[test]
    fn detects_english_explicit_memory_intent() {
        assert!(
            nudge_for_prompt("Please remember: when I say quick pass, only run focused tests.")
                .is_some()
        );
        assert!(nudge_for_prompt("From now on, prefer compact status summaries.").is_some());
    }

    #[test]
    fn broad_memory_or_negated_mentions_stay_noop() {
        assert!(nudge_for_prompt("Can you explain how session memory works?").is_none());
        assert!(nudge_for_prompt("I don't remember this error from yesterday.").is_none());
        assert!(nudge_for_prompt("不用记住这个临时 token。").is_none());
    }

    #[test]
    fn claude_output_surfaces_user_prompt_submit_nudge_as_context() {
        let mut result = nudge_for_prompt("remember this: use focused hook tests")
            .expect("explicit remember request should nudge");
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
                .contains("candidate preference/rule")
        );
    }
}
