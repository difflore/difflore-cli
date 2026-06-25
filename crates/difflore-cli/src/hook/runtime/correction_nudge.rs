const ENGLISH_POSITIVE_PHRASES: &[&str] = &[
    "actually use",
    "actually make it",
    "instead use",
    "prefer",
    "don't do that",
    "dont do that",
    "do not do that",
    "not that way",
    "that's wrong",
    "thats wrong",
    "you misunderstood",
    "i meant",
    "i said",
    "revert that",
    "undo that",
    "go back",
];

const ENGLISH_NEGATIVE_PHRASES: &[&str] = &[
    "how do i correct",
    "spell correction",
    "correction factor",
    "grammar correction",
    "autocorrect",
];

const CHINESE_POSITIVE_PHRASES: &[&str] = &[
    "不是这样",
    "不对",
    "错了",
    "应该用",
    "改用",
    "别这样",
    "不要这样",
    "我说的是",
    "我的意思是",
    "撤回",
    "还原",
];

const CHINESE_NEGATIVE_PHRASES: &[&str] = &["纠错算法", "拼写纠正", "语法纠正"];

pub(super) fn has_implicit_correction(prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return false;
    }

    let lower = prompt.to_ascii_lowercase();
    if contains_any_english_phrase(&lower, ENGLISH_NEGATIVE_PHRASES)
        || contains_any(prompt, CHINESE_NEGATIVE_PHRASES)
    {
        return false;
    }

    contains_any_english_phrase(&lower, ENGLISH_POSITIVE_PHRASES)
        || contains_any(prompt, CHINESE_POSITIVE_PHRASES)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_any_english_phrase(haystack: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| contains_english_phrase(haystack, needle))
}

fn contains_english_phrase(haystack: &str, needle: &str) -> bool {
    let mut search_from = 0;
    while let Some(relative_start) = haystack[search_from..].find(needle) {
        let start = search_from + relative_start;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack.as_bytes()[start - 1].is_ascii_alphanumeric()
                && haystack.as_bytes()[start - 1] != b'_';
        let after_ok = end == haystack.len()
            || !haystack.as_bytes()[end].is_ascii_alphanumeric()
                && haystack.as_bytes()[end] != b'_';
        if before_ok && after_ok {
            return true;
        }
        search_from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_english_implicit_corrections() {
        assert!(has_implicit_correction(
            "Not that way; use the typed helper instead."
        ));
        assert!(has_implicit_correction(
            "That's wrong, I meant keep the null branch."
        ));
        assert!(has_implicit_correction(
            "Please revert that and prefer the existing parser."
        ));
    }

    #[test]
    fn detects_chinese_implicit_corrections() {
        assert!(has_implicit_correction("不是这样，应该用现有 parser。"));
        assert!(has_implicit_correction("不对，我说的是保留 null 分支。"));
    }

    #[test]
    fn broad_mentions_stay_quiet() {
        assert!(!has_implicit_correction(
            "Can you explain spell correction algorithms?"
        ));
        assert!(!has_implicit_correction(
            "autocorrect keeps changing this word"
        ));
        assert!(!has_implicit_correction("纠错算法这里怎么实现？"));
    }
}
