use crate::models::{SkillRecord, SkillRepoRecord};

#[allow(clippy::many_single_char_names)] // reason: base64 nibbles a/b/c/d are conventional
pub(crate) fn decode_base64_lossy(input: &str) -> String {
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    // safe: alphabet has 64 entries; i fits in u8 trivially
    for (i, &ch) in alphabet.iter().enumerate() {
        lookup[ch as usize] = i as u8;
    }

    let mut bytes = Vec::new();
    let chars: Vec<u8> = cleaned.bytes().filter(|&b| b != b'=').collect();
    let mut i = 0;
    while i + 3 < chars.len() {
        let (a, b, c, d) = (
            lookup[chars[i] as usize],
            lookup[chars[i + 1] as usize],
            lookup[chars[i + 2] as usize],
            lookup[chars[i + 3] as usize],
        );
        if a == 255 || b == 255 {
            break;
        }
        bytes.push((a << 2) | (b >> 4));
        if c != 255 {
            bytes.push((b << 4) | (c >> 2));
        }
        if d != 255 {
            bytes.push((c << 6) | d);
        }
        i += 4;
    }
    let remaining = chars.len() - i;
    if remaining >= 2 {
        let a = lookup[chars[i] as usize];
        let b = lookup[chars[i + 1] as usize];
        if a != 255 && b != 255 {
            bytes.push((a << 2) | (b >> 4));
            if remaining >= 3 {
                let c = lookup[chars[i + 2] as usize];
                if c != 255 {
                    bytes.push((b << 4) | (c >> 2));
                }
            }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

#[derive(sqlx::FromRow)]
pub struct SkillRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) source: String,
    pub(crate) directory: String,
    pub(crate) version: String,
    pub(crate) description: String,
    pub(crate) r#type: String,
    pub(crate) engines: String,
    pub(crate) tags: String,
    pub(crate) trigger: Option<String>,
    pub(crate) check_prompt: Option<String>,
    pub(crate) repo_owner: Option<String>,
    pub(crate) repo_name: Option<String>,
    pub(crate) repo_branch: Option<String>,
    pub(crate) readme_url: Option<String>,
    pub(crate) enabled_for_codex: i64,
    pub(crate) enabled_for_claude: i64,
    pub(crate) enabled_for_gemini: i64,
    pub(crate) enabled_for_cursor: i64,
    pub(crate) installed_at: String,
    pub(crate) updated_at: String,
    pub(crate) origin: String,
}

impl From<SkillRow> for SkillRecord {
    fn from(r: SkillRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            source: r.source,
            directory: r.directory,
            version: r.version,
            description: r.description,
            r#type: r.r#type,
            engines: parse_engines_column(&r.engines),
            tags: serde_json::from_str(&r.tags).unwrap_or_default(),
            trigger: r.trigger,
            check_prompt: r.check_prompt,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            repo_branch: r.repo_branch,
            readme_url: r.readme_url,
            enabled_for_codex: r.enabled_for_codex != 0,
            enabled_for_claude: r.enabled_for_claude != 0,
            enabled_for_gemini: r.enabled_for_gemini != 0,
            enabled_for_cursor: r.enabled_for_cursor != 0,
            installed_at: r.installed_at,
            updated_at: r.updated_at,
            enforcement: None,
            origin: r.origin,
        }
    }
}

fn parse_engines_column(raw: &str) -> Vec<String> {
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(engines) => engines,
        Err(e) => {
            let fallback: Vec<String> = parse_list_value(raw)
                .into_iter()
                .filter(|engine| is_known_engine(engine))
                .collect();
            if fallback.is_empty() {
                eprintln!("warning: DiffLore could not read skills.engines; using claude.");
                if crate::env::debug_telemetry() {
                    eprintln!("[difflore] malformed skills.engines JSON: {e}");
                }
                vec!["claude".to_owned()]
            } else {
                if crate::env::debug_telemetry() {
                    eprintln!(
                        "[difflore] malformed skills.engines JSON ({e}); parsed legacy list syntax"
                    );
                }
                fallback
            }
        }
    }
}

fn is_known_engine(engine: &str) -> bool {
    matches!(engine, "claude" | "codex" | "gemini" | "cursor")
}

#[derive(sqlx::FromRow)]
pub struct SkillRepoRow {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) branch: String,
    pub(crate) enabled: i64,
    pub(crate) created_at: String,
}

impl From<SkillRepoRow> for SkillRepoRecord {
    fn from(r: SkillRepoRow) -> Self {
        Self {
            id: r.id,
            owner: r.owner,
            name: r.name,
            branch: r.branch,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

pub(crate) struct SkillFrontmatter {
    pub(crate) r#type: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) trigger: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) engines: Option<Vec<String>>,
    pub(crate) body: String,
}

pub(crate) fn parse_list_value(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner
        .split(',')
        .map(|s| {
            s.trim()
                .trim_matches(|c: char| c == '\'' || c == '"')
                .to_owned()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

pub(crate) fn parse_skill_frontmatter(content: &str) -> SkillFrontmatter {
    let mut fm = SkillFrontmatter {
        r#type: None,
        tags: None,
        trigger: None,
        version: None,
        engines: None,
        body: content.to_owned(),
    };

    let mut lines = content.lines().peekable();
    match lines.peek() {
        Some(line) if line.trim() == "---" => {
            lines.next();
        }
        _ => return fm,
    }

    let mut body_start = false;
    let mut body_lines = Vec::new();
    for line in lines {
        if body_start {
            body_lines.push(line);
        } else {
            let trimmed = line.trim();
            if trimmed == "---" {
                body_start = true;
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "type" => fm.r#type = Some(value.to_owned()),
                    "trigger" => fm.trigger = Some(value.to_owned()),
                    "version" => fm.version = Some(value.to_owned()),
                    "tags" => fm.tags = Some(parse_list_value(value)),
                    "engines" => fm.engines = Some(parse_list_value(value)),
                    _ => {}
                }
            }
        }
    }
    if body_start {
        fm.body = body_lines.join("\n");
    }

    fm
}
