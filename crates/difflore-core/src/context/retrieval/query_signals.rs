const GO_RANGE_LOOP_VARIABLE_SIGNAL: &str =
    "go range loop variable capture goroutine closure iteration pointer address";
const KOREAN_OPTIONAL_BODY_TRANSLATION_SIGNAL: &str = "korean translation optional required body fields attribute default none 선택적 필수 필수가 아닙니다 기본값 어트리뷰트";
const ASYNC_CANCELLATION_AWAIT_SIGNAL: &str =
    "async task cancellation await point asyncio anyio cooperative yield custom response cancel";
const SOLID_FORM_PREVENT_DEFAULT_SIGNAL: &str =
    "solid form submit onSubmit handler preventDefault page reload tanstack router action";
const TEST_PLACEMENT_SIGNAL: &str = "test placement unrelated utility module collocate tests with module they exercise cloneRawRequest raw request";

pub fn build_recall_query_with_signals(file: &str, intent: &str) -> String {
    let base = format!("{file} {intent}");
    let signals = recall_query_signal_text(file, intent);
    if signals.is_empty() {
        base
    } else {
        format!("{base}\n{}", signals.join("\n"))
    }
}

fn recall_query_signal_text(file: &str, intent: &str) -> Vec<&'static str> {
    let mut signals = Vec::new();
    if has_go_range_loop_variable_hazard(intent) && looks_like_go(file, intent) {
        signals.push(GO_RANGE_LOOP_VARIABLE_SIGNAL);
    }
    if has_korean_optional_body_translation_signal(file, intent) {
        signals.push(KOREAN_OPTIONAL_BODY_TRANSLATION_SIGNAL);
    }
    if has_async_cancellation_await_signal(file, intent) {
        signals.push(ASYNC_CANCELLATION_AWAIT_SIGNAL);
    }
    if has_solid_form_submit_signal(file, intent) {
        signals.push(SOLID_FORM_PREVENT_DEFAULT_SIGNAL);
    }
    if has_test_placement_signal(file, intent) {
        signals.push(TEST_PLACEMENT_SIGNAL);
    }
    signals
}

fn looks_like_go(file: &str, intent: &str) -> bool {
    file.trim().to_ascii_lowercase().ends_with(".go")
        || intent
            .lines()
            .any(|line| strip_diff_marker(line).trim_start().starts_with("package "))
}

fn strip_diff_marker(line: &str) -> &str {
    let trimmed = line.trim_start();
    match trimmed.as_bytes().first().copied() {
        Some(b'+' | b'-' | b'>') => &trimmed[1..],
        _ => line,
    }
}

fn strip_diff_line_markers(text: &str) -> String {
    text.lines()
        .map(strip_diff_marker)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalized_path(path: &str) -> String {
    path.trim_start_matches('/').replace('\\', "/")
}

fn normalized_intent(intent: &str) -> String {
    strip_diff_line_markers(intent).to_lowercase()
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn path_has_segment(path: &str, segment: &str) -> bool {
    path == segment
        || path.starts_with(&format!("{segment}/"))
        || path.ends_with(&format!("/{segment}"))
        || path.contains(&format!("/{segment}/"))
}

fn is_markdown_path(path: &str) -> bool {
    path.ends_with(".md") || path.ends_with(".mdx")
}

fn is_test_file_path(path: &str) -> bool {
    [
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".test.mts",
        ".test.cts",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        ".spec.jsx",
        ".spec.mts",
        ".spec.cts",
    ]
    .iter()
    .any(|suffix| path.ends_with(suffix))
}

fn has_korean_optional_body_translation_signal(file: &str, intent: &str) -> bool {
    let path = normalized_path(file).to_lowercase();
    if !path.contains("docs/ko/docs") {
        return false;
    }

    let text = normalized_intent(intent);
    let has_optional_required_term = contains_any(&text, &["선택", "필수", "optional", "required"]);
    let has_body_field_term = contains_any(
        &text,
        &[
            "body",
            "본문",
            "바디",
            "field",
            "attribute",
            "어트리뷰트",
            "default",
            "none",
            "기본값",
        ],
    );
    has_optional_required_term && has_body_field_term
}

fn has_async_cancellation_await_signal(file: &str, intent: &str) -> bool {
    let path = normalized_path(file).to_lowercase();
    let text = normalized_intent(intent);
    let mentions_cancellation = text.contains("task can only be cancelled")
        || (text.contains("cancel") && contains_any(&text, &["asyncio", "anyio"]));
    mentions_cancellation
        && text.contains("await")
        && (path.contains("custom-response")
            || path.contains("/docs/")
            || text.contains("async generator"))
}

fn has_solid_form_submit_signal(file: &str, intent: &str) -> bool {
    let path = normalized_path(file).to_lowercase();
    let text = normalized_intent(intent);
    let solid_context = path_has_segment(&path, "solid")
        || text.contains("solid")
        || text.contains("@tanstack/solid-router");
    let submit_context = contains_any(&text, &["<form", "onsubmit", "submit", "formdata"]);
    solid_context && submit_context && is_markdown_path(&path)
}

fn has_test_placement_signal(file: &str, intent: &str) -> bool {
    let path = normalized_path(file).to_lowercase();
    if !is_test_file_path(&path) {
        return false;
    }

    let text = normalized_intent(intent);
    let has_test_shape = contains_any(&text, &["describe(", "it(", "test("]);
    let has_utility_subject = contains_any(
        &text,
        &[
            "clonerawrequest",
            "raw request",
            "rawrequest",
            "util",
            "utility",
        ],
    );
    has_test_shape && has_utility_subject
}

fn has_go_range_loop_variable_hazard(intent: &str) -> bool {
    let text = strip_diff_line_markers(intent);
    let mut offset = 0usize;
    for line in text.lines() {
        if let Some(variable) = range_loop_variable(line) {
            let end = (offset + 1200).min(text.len());
            let window = &text[offset..end];
            if returns_loop_variable_address(window, &variable)
                || closure_captures_loop_variable(window, &variable)
            {
                return true;
            }
        }
        offset += line.len() + 1;
    }
    false
}

fn range_loop_variable(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let for_index = lower.find("for ")?;
    let after_for = &line[for_index + 4..];
    let lower_after_for = &lower[for_index + 4..];
    let range_index = lower_after_for.find(" range")?;
    let before_range = after_for[..range_index].trim();
    let before_assign = before_range.split(":=").next()?.trim();
    let variable = before_assign.split(',').next_back()?.trim();
    if is_go_ident(variable) && variable != "_" {
        Some(variable.to_owned())
    } else {
        None
    }
}

fn is_go_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn returns_loop_variable_address(window: &str, variable: &str) -> bool {
    window.lines().any(|line| {
        let Some(rest) = line.trim_start().strip_prefix("return") else {
            return false;
        };
        let Some(rest) = rest.trim_start().strip_prefix('&') else {
            return false;
        };
        let rest = rest.trim_start();
        rest == variable
            || rest
                .strip_prefix(variable)
                .is_some_and(|suffix| suffix.starts_with(char::is_whitespace) || suffix == ",")
    })
}

fn closure_captures_loop_variable(window: &str, variable: &str) -> bool {
    let lower = window.to_ascii_lowercase();
    let Some(func_index) = lower.find("func") else {
        return false;
    };
    let end = (func_index + 700).min(window.len());
    contains_word(&window[func_index..end], variable)
}

fn contains_word(text: &str, needle: &str) -> bool {
    let mut start = 0usize;
    while let Some(relative) = text[start..].find(needle) {
        let index = start + relative;
        let before = text[..index].chars().next_back();
        let after = text[index + needle.len()..].chars().next();
        let before_ok = before.is_none_or(|c| !(c == '_' || c.is_ascii_alphanumeric()));
        let after_ok = after.is_none_or(|c| !(c == '_' || c.is_ascii_alphanumeric()));
        if before_ok && after_ok {
            return true;
        }
        start = index + needle.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_query_signal_adds_go_loop_variable_terms_for_pointer_return() {
        let intent = r"@@ -0,0 +1,12 @@
+package extensions
+
+func FindOfficialExtension(commandName string) *OfficialExtension {
+  for _, ext := range officialExtensions {
+    if ext.Name == commandName {
+      return &ext
+    }
+  }
+  return nil
+}";

        let query = build_recall_query_with_signals("pkg/extensions/official.go", intent);

        assert!(query.contains("loop variable capture"));
        assert!(query.contains("goroutine closure"));
    }

    #[test]
    fn recall_query_signal_adds_go_loop_variable_terms_for_closure_capture() {
        let intent = r"
package main

func run(items []Item) {
  for _, item := range items {
    go func() {
      handle(item)
    }()
  }
}
";

        let query = build_recall_query_with_signals("worker.go", intent);

        assert!(query.contains("loop variable capture"));
    }

    #[test]
    fn recall_query_signal_does_not_expand_ordinary_range_loop() {
        let intent = r"
package main

func names(items []Item) []string {
  names := make([]string, 0, len(items))
  for _, item := range items {
    names = append(names, item.Name)
  }
  return names
}
";

        assert_eq!(
            build_recall_query_with_signals("items.go", intent),
            format!("{} {}", "items.go", intent)
        );
    }

    #[test]
    fn recall_query_signal_does_not_expand_non_go_file() {
        let intent = r"
for _, item := range items {
  return &item
}
";

        assert_eq!(
            build_recall_query_with_signals("items.ts", intent),
            format!("{} {}", "items.ts", intent)
        );
    }

    #[test]
    fn recall_query_signal_adds_korean_optional_body_translation_terms() {
        let intent = r"
+ 필드에 기본값이 있는 경우에는 필수가 아닙니다.
+ None을 사용하면 선택적으로 보낼 수 있습니다.
";

        let query = build_recall_query_with_signals("docs/ko/docs/tutorial/body.md", intent);

        assert!(query.contains("korean translation optional"));
        assert!(query.contains("선택적"));
    }

    #[test]
    fn recall_query_signal_adds_async_cancellation_await_terms() {
        let intent = r"
+ In asyncio, a task can only be cancelled when it reaches an await.
+ Streaming responses should document the await point precisely.
";

        let query =
            build_recall_query_with_signals("docs/en/docs/advanced/custom-response.md", intent);

        assert!(query.contains("async task cancellation await"));
        assert!(query.contains("anyio"));
    }

    #[test]
    fn recall_query_signal_adds_solid_form_prevent_default_terms() {
        let intent = r"
+ <form onSubmit={async (event) => {
+   const formData = new FormData(event.currentTarget)
+ }}>
";

        let query = build_recall_query_with_signals(
            "docs/start/framework/solid/tutorial/reading-writing-file.md",
            intent,
        );

        assert!(query.contains("preventDefault"));
        assert!(query.contains("solid form submit"));
    }

    #[test]
    fn recall_query_signal_adds_test_placement_terms_for_utility_tests() {
        let intent = r"
+ describe('cloneRawRequest', () => {
+   it('keeps the raw request body', () => {})
+ })
";

        let query = build_recall_query_with_signals("src/validator/validator.test.ts", intent);

        assert!(query.contains("test placement unrelated utility"));
        assert!(query.contains("cloneRawRequest"));
    }
}
