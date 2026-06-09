pub const PRIVATE_REDACTION: &str = "[redacted private content]";

/// Marker substituted in place of every redacted secret by [`redact_secrets`].
/// Kept byte-for-byte identical to the cloud's `SECRET_REDACTION_PLACEHOLDER`
/// (`redact-secrets.ts`) so a rule that round-trips through either side reads
/// the same.
pub const SECRET_REDACTION_PLACEHOLDER: &str = "‹redacted-secret›";

/// Conservative pre-persist secret redaction for locally-drafted rule text.
///
/// This is the Rust analogue of the cloud's `redactSecrets` in
/// `difflore-cloud/src/lib/redact-secrets.ts`; it mirrors the SAME secret
/// classes so a rule drafted locally is scrubbed before it is written to the
/// SQLite skills store (and lazily embedded), exactly as the cloud scrubs
/// before persisting/embedding a candidate. The classes, in priority order:
///
///   1. Provider-prefixed credentials + JWTs — redacted on shape alone
///      (`gh[opsu]_…`, `github_pat_…`, `sk-…`, `xox[baprs]-…`, `AKIA…`,
///      JWT `eyJ….….…`).
///   2. `Bearer <token>` (HTTP Authorization style) — unless the token is a
///      plain code reference.
///   3. `<keyword> [:=] <value>` assignments for api_key / access_token /
///      refresh_token / id_token / auth_token / bearer_token / client_secret /
///      webhook_secret / secret / password / passwd / pwd — redacted ONLY when
///      the value both carries secret-like entropy AND is not a code reference.
///
/// Conservative by design: it runs over real review prose and quoted code
/// snippets, so a false positive silently corrupts a legitimate rule. The
/// keyword-assignment class therefore never fires on `config.apiKey`,
/// `process.env.API_KEY`, `getToken()`, or a plain identifier; the prefix/JWT
/// classes fire only on their distinctive high-entropy shape. Plain prose, git
/// SHAs, and UUIDs are left untouched (see the unit tests).
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if at_word_boundary(&chars, i) {
            // 1) Provider-prefixed credential / JWT — redact on shape.
            if let Some(end) = match_known_prefix_secret(&chars, i) {
                out.push_str(SECRET_REDACTION_PLACEHOLDER);
                i = end;
                continue;
            }
            // 2) `Bearer <token>` — redact unless the token is a code ref.
            if let Some((prefix_end, token_end)) = match_bearer_secret(&chars, i) {
                let value: String = chars[prefix_end..token_end].iter().collect();
                if !looks_like_code_reference(&value) {
                    out.extend(chars[i..prefix_end].iter());
                    out.push_str(SECRET_REDACTION_PLACEHOLDER);
                    i = token_end;
                    continue;
                }
            }
            // 3) `<keyword> [:=] [quote] <token> [quote]` — redact only a
            //    high-entropy, non-reference value.
            if let Some(m) = match_named_secret_assign(&chars, i) {
                let value: String = chars[m.value_start..m.value_end].iter().collect();
                if !looks_like_code_reference(&value) && has_secret_entropy(&value) {
                    // `chars[i..value_start]` already carries the keyword,
                    // operator, whitespace, AND the opening quote (value_start
                    // sits just past it). Emit that, the placeholder, then a
                    // symmetric closing quote — mirroring the cloud's
                    // `${prefix}${openQuote}${PLACEHOLDER}${openQuote}`. The
                    // ORIGINAL closing quote is consumed via `match_end`.
                    out.extend(chars[i..m.value_start].iter());
                    out.push_str(SECRET_REDACTION_PLACEHOLDER);
                    if let Some(q) = m.open_quote {
                        out.push(q);
                    }
                    i = m.match_end;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A char is part of the `\w`-class token alphabet shared by the cloud regexes
/// (`[\w.~+/=-]` plus the prefix/JWT alphabets). Used for `\b` boundary checks
/// so we only start a match at a real token boundary, never mid-identifier.
const fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '~' | '+' | '/' | '=' | '-')
}

/// `\w` (word) char for the `\b` boundaries the cloud regexes use: ASCII
/// alphanumeric or underscore. A match may only begin where the previous char
/// is NOT a word char (start-of-string counts as a boundary).
const fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn at_word_boundary(chars: &[char], i: usize) -> bool {
    i == 0 || !is_word_char(chars[i - 1])
}

/// Length (in chars) of the maximal secret-token run `[\w.~+/=-]+` starting at
/// `start`. Mirrors the cloud `SECRET_TOKEN = [\w.~+/=-]{12,}` (the `{12,}`
/// length gate is applied by callers).
fn secret_token_len(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while end < chars.len() && is_token_char(chars[end]) {
        end += 1;
    }
    end - start
}

/// Try to match a provider-prefixed credential or JWT at `start`, returning the
/// end index (exclusive) on success. Each arm also enforces the trailing `\b`
/// the cloud regex requires, so `AKIA…` embedded in a longer token is rejected.
fn match_known_prefix_secret(chars: &[char], start: usize) -> Option<usize> {
    // gh[opsu]_[A-Za-z0-9]{20,}
    if let Some(&[g, h, t, u]) = chars.get(start..start + 4) {
        if g == 'g' && h == 'h' && matches!(t, 'o' | 'p' | 's' | 'u') && u == '_' {
            if let Some(end) = match_prefix_run(chars, start + 4, 20, |c| c.is_ascii_alphanumeric())
            {
                return Some(end);
            }
        }
    }
    // github_pat_[A-Za-z0-9_]{20,} (case-sensitive, like the cloud arm).
    if starts_with_chars(chars, start, "github_pat_") {
        if let Some(end) = match_prefix_run(chars, start + "github_pat_".len(), 20, |c| {
            c.is_ascii_alphanumeric() || c == '_'
        }) {
            return Some(end);
        }
    }
    // sk-[A-Za-z0-9]{20,} (case-sensitive, like the cloud arm).
    if starts_with_chars(chars, start, "sk-") {
        if let Some(end) = match_prefix_run(chars, start + "sk-".len(), 20, |c| {
            c.is_ascii_alphanumeric()
        }) {
            return Some(end);
        }
    }
    // xox[baprs]-[A-Za-z0-9-]{20,}
    if let Some(&[x, o, x2, kind, dash]) = chars.get(start..start + 5) {
        if x == 'x'
            && o == 'o'
            && x2 == 'x'
            && matches!(kind, 'b' | 'a' | 'p' | 'r' | 's')
            && dash == '-'
        {
            if let Some(end) = match_prefix_run(chars, start + 5, 20, |c| {
                c.is_ascii_alphanumeric() || c == '-'
            }) {
                return Some(end);
            }
        }
    }
    // AKIA[0-9A-Z]{16} — case-sensitive prefix, then exactly 16 uppercase/digit
    // chars and a trailing boundary.
    if starts_with_chars(chars, start, "AKIA") {
        let body_start = start + 4;
        let end = body_start + 16;
        if end <= chars.len()
            && chars[body_start..end]
                .iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && (end >= chars.len() || !is_word_char(chars[end]))
        {
            return Some(end);
        }
    }
    // eyJ[\w-]{10,}\.[\w-]{10,}\.[\w-]{10,} — JWT (three base64url segments).
    if starts_with_chars(chars, start, "eyJ") {
        if let Some(end) = match_jwt(chars, start) {
            return Some(end);
        }
    }
    None
}

/// True when `chars[start..]` begins with the ASCII `prefix` (case-sensitive),
/// compared char-by-char so no allocation is needed per probe.
fn starts_with_chars(chars: &[char], start: usize, prefix: &str) -> bool {
    let mut idx = start;
    for pc in prefix.chars() {
        if chars.get(idx) != Some(&pc) {
            return false;
        }
        idx += 1;
    }
    true
}

/// Case-INSENSITIVE variant of [`starts_with_chars`] for the keyword-assignment
/// class (the cloud regex carries the `i` flag).
fn starts_with_chars_ci(chars: &[char], start: usize, prefix: &str) -> bool {
    let mut idx = start;
    for pc in prefix.chars() {
        match chars.get(idx) {
            Some(c) if c.eq_ignore_ascii_case(&pc) => {}
            _ => return false,
        }
        idx += 1;
    }
    true
}

/// Match a `prefix`-run of at least `min` chars satisfying `pred` beginning at
/// `body_start`, with a trailing `\b`. Returns the end index on success.
fn match_prefix_run(
    chars: &[char],
    body_start: usize,
    min: usize,
    pred: impl Fn(char) -> bool,
) -> Option<usize> {
    let mut end = body_start;
    while end < chars.len() && pred(chars[end]) {
        end += 1;
    }
    // The run stops at the first char failing `pred`. Every arm's `pred`
    // already accepts the full `\w`-superset its `\b` cares about (alnum, `_`,
    // `-`), so stopping here IS the trailing word-boundary the cloud regex
    // requires — no extra check needed.
    (end - body_start >= min).then_some(end)
}

/// JWT: three `[\w-]{10,}` segments separated by literal dots, starting at the
/// `eyJ` header. Enforces the trailing `\b`.
fn match_jwt(chars: &[char], start: usize) -> Option<usize> {
    let seg = |from: usize| -> Option<usize> {
        let mut end = from;
        while end < chars.len() && (is_word_char(chars[end]) || chars[end] == '-') {
            end += 1;
        }
        (end - from >= 10).then_some(end)
    };
    let s1 = seg(start)?;
    if chars.get(s1) != Some(&'.') {
        return None;
    }
    let s2 = seg(s1 + 1)?;
    if chars.get(s2) != Some(&'.') {
        return None;
    }
    let s3 = seg(s2 + 1)?;
    if s3 < chars.len() && is_word_char(chars[s3]) {
        return None;
    }
    Some(s3)
}

/// `Bearer\s+<token>` — returns `(prefix_end, token_end)` where `prefix_end` is
/// the index just past the whitespace (start of the token). The token is the
/// `[\w.~+/=-]{12,}` run; trailing `\b` is implied because the run stops at the
/// first non-token char.
fn match_bearer_secret(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let head: String = chars
        .get(start..(start + 6).min(chars.len()))?
        .iter()
        .collect();
    if head != "Bearer" {
        return None;
    }
    let mut j = start + 6;
    let ws_start = j;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    if j == ws_start {
        return None; // require at least one whitespace char (`\s+`)
    }
    let len = secret_token_len(chars, j);
    if len < 12 {
        return None;
    }
    Some((j, j + len))
}

struct NamedAssignMatch {
    value_start: usize,
    value_end: usize,
    open_quote: Option<char>,
    match_end: usize,
}

/// `<keyword>\s*[:=]\s*["'`]?<token>["'`]?` (case-insensitive keyword). Returns
/// the value span, the optional opening quote (re-emitted around the
/// placeholder so surrounding syntax survives), and the overall match end.
fn match_named_secret_assign(chars: &[char], start: usize) -> Option<NamedAssignMatch> {
    const KEYWORDS: &[&str] = &[
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "accesstoken",
        "access-token",
        "refresh_token",
        "refreshtoken",
        "refresh-token",
        "id_token",
        "idtoken",
        "id-token",
        "auth_token",
        "authtoken",
        "auth-token",
        "bearer_token",
        "bearertoken",
        "bearer-token",
        "client_secret",
        "clientsecret",
        "client-secret",
        "webhook_secret",
        "webhooksecret",
        "webhook-secret",
        "secret",
        "password",
        "passwd",
        "pwd",
    ];
    // Longest keyword first so `client_secret` wins over `secret`.
    let kw_len = KEYWORDS
        .iter()
        .filter(|kw| starts_with_chars_ci(chars, start, kw))
        .map(|kw| kw.chars().count())
        .max()?;
    let mut j = start + kw_len;
    // Reject if the keyword is only a prefix of a longer identifier
    // (`secretariat`, `passwords`): the next char must not be a word char.
    if j < chars.len() && is_word_char(chars[j]) {
        return None;
    }
    // `\s*` before the operator.
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    if !matches!(chars.get(j), Some(':' | '=')) {
        return None;
    }
    j += 1;
    // `\s*` after the operator.
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    let open_quote = match chars.get(j) {
        Some(c @ ('"' | '\'' | '`')) => {
            let q = *c;
            j += 1;
            Some(q)
        }
        _ => None,
    };
    let value_start = j;
    let len = secret_token_len(chars, value_start);
    if len < 12 {
        return None;
    }
    let value_end = value_start + len;
    let mut match_end = value_end;
    // Optional closing quote (the cloud captures `["'`]?` but does not require
    // it to match the opener); consume one if present.
    if matches!(chars.get(match_end), Some('"' | '\'' | '`')) {
        match_end += 1;
    }
    Some(NamedAssignMatch {
        value_start,
        value_end,
        open_quote,
        match_end,
    })
}

/// True when a keyword-assignment / Bearer value is plainly a code reference
/// rather than a literal secret — e.g. `config.apiKey`, `process.env.API_KEY`,
/// `req.body.clientSecret`, `getPassword()`, or a plain word identifier. Mirrors
/// the cloud `looksLikeCodeReference`. A high-entropy token like `A1b2C3d4E5f6`
/// has interior digits and so fails the word-identifier arm, falling through as
/// a secret.
fn looks_like_code_reference(value: &str) -> bool {
    // Call / index expression: getPassword(), tokens[0].
    if value.contains(['(', ')', '[', ']']) {
        return true;
    }
    // Dotted member access: foo.bar.baz (each segment a JS identifier).
    if is_dotted_member_access(value) {
        return true;
    }
    // Word-shaped identifier (letters/underscores/`$`, optional TRAILING
    // digits): apiKey, API_KEY, token2. Interior digits fall through.
    if is_word_identifier(value) {
        return true;
    }
    false
}

/// `^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)+$` — at least one dot, each segment
/// a JS identifier.
fn is_dotted_member_access(value: &str) -> bool {
    if !value.contains('.') {
        return false;
    }
    let mut segments = value.split('.');
    let mut count = 0usize;
    for seg in &mut segments {
        if !is_js_identifier(seg) {
            return false;
        }
        count += 1;
    }
    count >= 2
}

/// `^[A-Za-z_$][\w$]*$` — a single JS identifier segment.
fn is_js_identifier(seg: &str) -> bool {
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// `^[A-Za-z_$][A-Za-z_$]*\d*$` — letters/underscores/`$`, then optional
/// TRAILING digits only (no interior digits). `apiKey`, `API_KEY`, `token2`.
fn is_word_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    let mut seen_digit = false;
    for c in chars {
        if c.is_ascii_digit() {
            seen_digit = true;
        } else if seen_digit {
            // A non-digit after a digit means interior digits → not a plain
            // identifier (e.g. `A1b2`).
            return false;
        } else if !(c.is_ascii_alphabetic() || c == '_' || c == '$') {
            return false;
        }
    }
    true
}

/// True when a keyword-assignment value carries secret-like entropy: a
/// letter+digit mix, base64 padding/separators at length, or a very long opaque
/// token. Mirrors the cloud `hasSecretEntropy`. Plain words and short
/// references are rejected so `password = secret` is never redacted.
fn has_secret_entropy(value: &str) -> bool {
    let has_letter = value.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    if has_letter && has_digit {
        return true;
    }
    let has_base64_punct = value.contains(['+', '/', '=']);
    let len = value.chars().count();
    if has_base64_punct && len >= 16 {
        return true;
    }
    len >= 40
}

const PRIVATE_TAG_PAIRS: &[(&str, &str)] = &[
    ("<private>", "</private>"),
    ("<secret>", "</secret>"),
    ("<sensitive>", "</sensitive>"),
];

pub fn strip_private_tagged_regions(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some((start, open, close)) = next_private_open_tag(&lower, cursor) {
        out.push_str(&input[cursor..start]);
        out.push_str(PRIVATE_REDACTION);

        let content_start = start + open.len();
        cursor = match lower[content_start..].find(close) {
            Some(rel_end) => content_start + rel_end + close.len(),
            None => input.len(),
        };
    }

    out.push_str(&input[cursor..]);
    out
}

pub fn redact_secretish_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut token = String::new();

    for ch in input.chars() {
        if ch.is_whitespace() {
            push_redacted_token(&mut out, &token);
            token.clear();
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    push_redacted_token(&mut out, &token);
    out
}

fn push_redacted_token(out: &mut String, token: &str) {
    if token.is_empty() {
        return;
    }
    let trimmed = token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | ':' | ')' | '(' | ']' | '[' | '{' | '}'
        )
    });
    if looks_secretish(trimmed) {
        let prefix_len = token.find(trimmed).unwrap_or(0);
        let suffix_start = prefix_len + trimmed.len();
        out.push_str(&token[..prefix_len]);
        if let Some((key, _)) = trimmed.split_once('=') {
            out.push_str(key);
            out.push('=');
        }
        out.push_str(PRIVATE_REDACTION);
        out.push_str(&token[suffix_start..]);
    } else {
        out.push_str(token);
    }
}

fn looks_secretish(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let value = lower
        .split_once('=')
        .map_or(lower.as_str(), |(_, value)| value);
    if value.starts_with("sk-") && value.len() >= 16 {
        return true;
    }
    if value.starts_with("ghp_")
        || value.starts_with("gho_")
        || value.starts_with("ghu_")
        || value.starts_with("ghs_")
        || value.starts_with("github_pat_")
    {
        return value.len() >= 20;
    }
    let raw = token
        .split_once('=')
        .map_or(token, |(_, value)| value)
        .trim();
    raw.len() >= 20
        && raw.starts_with("AKIA")
        && raw
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn next_private_open_tag(lower: &str, cursor: usize) -> Option<(usize, &str, &str)> {
    PRIVATE_TAG_PAIRS
        .iter()
        .filter_map(|(open, close)| {
            lower[cursor..]
                .find(open)
                .map(|rel| (cursor + rel, *open, *close))
        })
        .min_by_key(|(start, _, _)| *start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_private_tagged_regions_redacts_known_tags() {
        let input = "keep <private>token=abc</private> and <secret>sk-123</secret>";

        let out = strip_private_tagged_regions(input);

        assert_eq!(
            out,
            "keep [redacted private content] and [redacted private content]"
        );
        assert!(!out.contains("token=abc"));
        assert!(!out.contains("sk-123"));
    }

    #[test]
    fn strip_private_tagged_regions_is_case_insensitive() {
        let out = strip_private_tagged_regions("a <Sensitive>customer</SENSITIVE> b");

        assert_eq!(out, "a [redacted private content] b");
    }

    #[test]
    fn strip_private_tagged_regions_redacts_unclosed_tag_to_end() {
        let out = strip_private_tagged_regions("safe <private>do not store");

        assert_eq!(out, "safe [redacted private content]");
    }

    #[test]
    fn redact_secretish_tokens_redacts_common_raw_tokens() {
        let out = redact_secretish_tokens(
            "openai=sk-proj-abcdefghijklmnopqrstuvwxyz ghp_abcdefghijklmnopqrstuvwxyz AKIAABCDEFGHIJKLMNOP",
        );

        assert_eq!(
            out,
            "openai=[redacted private content] [redacted private content] [redacted private content]"
        );
    }

    #[test]
    fn redact_secretish_tokens_keeps_short_false_positives() {
        let out = redact_secretish_tokens("use sk-test in docs and ticket ghp_short");

        assert_eq!(out, "use sk-test in docs and ticket ghp_short");
    }

    // ── redact_secrets: one assertion per secret class, plus guards ──────────

    const M: &str = SECRET_REDACTION_PLACEHOLDER;

    /// Assert the input is fully scrubbed: the placeholder appears and no
    /// substring of the original secret survives.
    fn assert_redacted(input: &str, secret: &str) {
        let out = redact_secrets(input);
        assert!(out.contains(M), "expected redaction in {out:?}");
        assert!(
            !out.contains(secret),
            "secret {secret:?} leaked through: {out:?}"
        );
    }

    /// Assert the input is returned byte-for-byte (no false positive).
    fn assert_untouched(input: &str) {
        let out = redact_secrets(input);
        assert_eq!(out, input, "false-positive redaction");
        assert!(!out.contains(M), "false-positive redaction: {out:?}");
    }

    #[test]
    fn redacts_github_token_classes() {
        // gh[opsu]_ OAuth / PAT / app / refresh tokens.
        for tok in [
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "gho_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123",
            "ghu_0123456789abcdefghijklmnopqrst",
            "ghs_abcdefghijklmnopqrstuvwxyzABCD",
        ] {
            assert_redacted(&format!("token is {tok} here"), tok);
        }
        // github_pat_ fine-grained PAT.
        let pat = "github_pat_11ABCDE0123456789abcdefABCDEF";
        assert_redacted(&format!("see {pat} end"), pat);
    }

    #[test]
    fn redacts_openai_style_sk_key() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz1234";
        assert_redacted(&format!("key={key}"), key);
    }

    #[test]
    fn redacts_slack_xox_token() {
        // Synthetic, not a real token: matches the redaction regex
        // (`xox[baprs]-[A-Za-z0-9-]{20,}`) without looking like a real Slack
        // token, so secret scanners don't false-positive on this fixture.
        let tok = "xoxb-EXAMPLEONLY-NOTAREALTOKEN-PLACEHOLDER";
        assert_redacted(&format!("slack {tok} token"), tok);
    }

    #[test]
    fn redacts_aws_akia_key() {
        // AKIA + exactly 16 uppercase/digit chars.
        let key = "AKIAIOSFODNN7EXAMPLE";
        assert_redacted(&format!("aws id {key} here"), key);
    }

    #[test]
    fn redacts_jwt_eyj_token() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                   eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
                   dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_redacted(&format!("jwt {jwt} end"), jwt);
    }

    #[test]
    fn redacts_bearer_token() {
        let tok = "abcdef1234567890XYZ";
        let out = redact_secrets(&format!("Authorization: Bearer {tok}"));
        // The `Bearer ` prefix is preserved; only the token is scrubbed.
        assert_eq!(out, format!("Authorization: Bearer {M}"));
    }

    #[test]
    fn redacts_named_secret_assignments_preserving_quotes() {
        // High-entropy value (letter+digit mix) behind each keyword family.
        let out = redact_secrets(r#"api_key = "A1b2C3d4E5f6G7h8""#);
        assert_eq!(out, format!(r#"api_key = "{M}""#));

        assert_redacted("access_token: Zx9Yw8Vu7Ts6Rq5Po4", "Zx9Yw8Vu7Ts6Rq5Po4");
        assert_redacted("client_secret='Q1w2E3r4T5y6U7i8'", "Q1w2E3r4T5y6U7i8");
        assert_redacted("password=Hunter2Hunter2Hunter2", "Hunter2Hunter2Hunter2");
        // Long opaque base64-ish value with no digits still trips entropy.
        assert_redacted(
            "webhook_secret = AbCdEfGhIjKlMnOpQr/StUvWxYz+aBcDeFgHiJkLmNo",
            "AbCdEfGhIjKlMnOpQr/StUvWxYz+aBcDeFgHiJkLmNo",
        );
    }

    #[test]
    fn guard_code_reference_value_is_not_redacted() {
        // The canonical false positive: assigning from a config object.
        assert_untouched("const apiKey = config.apiKey");
        assert_untouched("token = process.env.API_KEY");
        assert_untouched("const secret = req.body.clientSecret");
        // Call / index expressions are code references too.
        assert_untouched("password = getPassword()");
        // Plain identifier value (no interior digits) is a reference.
        assert_untouched("api_key = apiKeyVariable");
        // `Bearer <identifier>` is a code reference, not a literal token.
        assert_untouched("Bearer authorizationToken");
    }

    #[test]
    fn guard_low_entropy_assignment_is_not_redacted() {
        // Plain word value, no letter+digit mix / base64 / length → kept.
        assert_untouched("password = secret");
        assert_untouched("secret: changeme");
    }

    #[test]
    fn guard_git_sha_is_not_redacted() {
        // A 40-char hex commit sha carries no keyword/prefix → left intact.
        assert_untouched("fixed in commit a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0");
    }

    #[test]
    fn guard_uuid_is_not_redacted() {
        assert_untouched("run id 550e8400-e29b-41d4-a716-446655440000 completed");
    }

    #[test]
    fn guard_normal_prose_is_not_redacted() {
        assert_untouched("Please validate the request body before returning a 413 status.");
        assert_untouched("Add a regression test that asserts the panic is no longer reachable.");
    }

    #[test]
    fn guard_keyword_substring_of_identifier_is_not_redacted() {
        // `secret` is a prefix of `secretariat`; must not trigger the keyword
        // class (no `\s*[:=]` follows the keyword boundary).
        assert_untouched("the secretariat: A1b2C3d4E5f6 reviewed it");
    }

    #[test]
    fn redacts_only_the_secret_inside_surrounding_prose() {
        let key = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let out = redact_secrets(&format!("Reviewer pasted {key} into the PR — rotate it."));
        assert_eq!(out, format!("Reviewer pasted {M} into the PR — rotate it."));
    }
}
