use serde_json::{Value, json};

pub(super) use super::skill_docs::{
    DIFFLORE_ONBOARD_SKILL_MD, KNOWLEDGE_AGENT_SKILL_MD, RULE_DIFF_SKILL_MD, RULE_GAP_SKILL_MD,
    RULE_JOURNEY_SKILL_MD, RULE_SEARCH_SKILL_MD, RULE_WHY_FIRED_SKILL_MD, SESSION_RECAP_SKILL_MD,
    SMART_EXPLORE_SKILL_MD,
};

pub(super) fn tools_list() -> Value {
    json!([
        {
            "name": "search_rules",
            "description": "Compact memory search. Returns rule ids/titles/origins plus match reasons before fetching details. Memory is scoped to the current git remotes; pass `repo_full_name` (repo namespace path such as GitHub owner/repo or GitLab group/project) when auto-detection is unavailable. Results are deterministically ordered (strict file-pattern hit, then relative-score band, then source priority manual > team > pr_review > extracted > conversation) and each carries a compact `why` ranking explanation (e.g. `strict-hit; band 9/10; source manual`). When team review history is available, results include citedCount and trustRate so agents can prefer rules that led to accepted edits. Use with get_rules to expand only matched rules.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Current file path (enables file-pattern cascade)"
                    },
                    "intent": {
                        "type": "string",
                        "description": "What the user is trying to do"
                    },
                    "repo_full_name": {
                        "type": "string",
                        "description": "GitHub `owner/repo` for the current project. Omit only when DiffLore can detect the current repo from git remotes."
                    },
                    "top_k": {
                        "type": "number",
                        "default": 5,
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum number of matched rules to return; default 5."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional agent session id used only for local DiffLore flywheel observation correlation."
                    }
                },
                "required": ["intent"]
            }
        },
        {
            "name": "get_rules",
            "description": "Fetch full rule text + examples by ID. Use after search_rules to expand only the matched rules you need. Batch multiple IDs in one call. Pass the current file path when editing so DiffLore can connect the rule to that file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string", "maxLength": 128 },
                        "minItems": 1,
                        "maxItems": 20
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional repo-relative file path being edited; helps DiffLore connect fetched rule details to that file."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional agent session id used only for local DiffLore flywheel observation correlation."
                    }
                },
                "required": ["ids"]
            }
        },
        {
            "name": "get_past_verdicts",
            "description": "Search team review history (WHAT the team decided on similar code before). Memory is scoped to the current repo/project only; pass `repo_full_name` (repo namespace path such as GitHub owner/repo or GitLab group/project) when auto-detection is unavailable. Pass `file` (the path you're editing) so DiffLore can prioritize matching file patterns and show useful gaps for that file. Use this to cite concrete prior decisions; use `rule_timeline` when you need the *why this rule exists* narrative for a specific rule.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query describing the code pattern"
                    },
                    "repo_full_name": {
                        "type": "string",
                        "description": "GitHub `owner/repo` for the current project. Omit only when DiffLore can detect the current repo from git origin."
                    },
                    "file": {
                        "type": "string",
                        "description": "Repo-relative path of the file you're editing (e.g. `src/auth/session.ts`). When supplied, DiffLore prioritizes rules whose `file_patterns` match this path first; without it, ordering falls back to overall relevance. Also helps the dashboard show where memory is missing."
                    },
                    "top_k": {
                        "type": "number",
                        "default": 10,
                        "minimum": 1,
                        "maximum": 10,
                        "description": "Maximum past verdicts to return; default 10."
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "rule_timeline",
            "description": "Chronological event stream for ONE rule - why it exists, how it got reinforced. Returns compact history rows for creation/update/example/feedback context. Use when the user asks 'where did this rule come from?' or you need team review history for a citation. Complements `get_past_verdicts` (timeline = why this rule; verdicts = what did we decide before).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rule_id": {
                        "type": "string",
                        "description": "Skill/rule id (e.g. `conv-my-rule-abc12345`). Accepts the id returned by `remember_rule` / `search_rules`."
                    },
                    "depth_before": {
                        "type": "number",
                        "default": 5,
                        "minimum": 0,
                        "maximum": 20,
                        "description": "Events before the focal timestamp (rule's created_at). Capped at 20."
                    },
                    "depth_after": {
                        "type": "number",
                        "default": 5,
                        "minimum": 0,
                        "maximum": 20,
                        "description": "Events after the focal timestamp. Capped at 20."
                    }
                },
                "required": ["rule_id"]
            }
        },
        {
            "name": "remember_rule",
            "description": "Persist a coding rule the user just stated so it survives this conversation and gets remembered in future agent edits + PR reviews. Call WHENEVER the user signals intent to make a rule stick (\"remember this\", \"from now on\", \"don't do X again\", \"always require tests before merge\", \"make this a rule\"). Pass `title` as a short imperative and `body` containing the user's verbatim reasoning - the WHY, not just what. Returns the saved rule_id; echo it back so the user knows the rule landed. Saying \"got it\" without calling this tool drops the rule the moment the session ends. Full trigger guide at difflore://skills/remember_rule.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short imperative title (80 chars or fewer). E.g. \"Avoid Promise.race for timeout in fetch wrappers\"."
                    },
                    "body": {
                        "type": "string",
                        "description": "Full natural-language explanation. Include WHY, not just what - quote the user's reason if they gave one. Multi-paragraph OK."
                    },
                    "file_patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional glob patterns the rule applies to (e.g. [\"**/*.ts\", \"src/api/**\"]). Omit for repo-wide rules. Drives strict file-pattern cascade at recall time."
                    },
                    "bad_code": {
                        "type": "string",
                        "description": "Optional snippet of the offending pattern. Pair with `good_code`."
                    },
                    "good_code": {
                        "type": "string",
                        "description": "Optional snippet of the corrected version. Pair with `bad_code`."
                    },
                    "severity": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Optional severity hint. Defaults to medium."
                    }
                },
                "required": ["title", "body"]
            }
        },
        {
            "name": "plan_pr",
            "description": "Predict the influence scope BEFORE editing: given an issue/PR description (`intent`), returns the expected file count, file-category mix, and the closest historical PRs from this team's review history. Use this to avoid silently under-completing - when the team's prior pattern for similar work touches 4+ files, finishing at 2 is the failure mode this prevents. Falls back to an empty prediction with a hint when no local PR review data exists - run `difflore import-reviews` to populate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "Issue/PR description text. Title + first paragraph of the body works well."
                    },
                    "top_k": {
                        "type": "number",
                        "default": 5,
                        "minimum": 1,
                        "maximum": 20,
                        "description": "How many nearest historical PRs to use for the prediction."
                    }
                },
                "required": ["intent"]
            }
        }
    ])
}

pub(super) fn resources_list() -> Value {
    json!([
        {
            "uri": "difflore://rules/active",
            "name": "Active Rules",
            "description": "All active rules for the current project, formatted as Markdown",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/remember_rule",
            "name": "remember_rule trigger guide",
            "description": "Full guide for when to call the remember_rule MCP tool, with trigger phrases and anti-patterns.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/rule-search",
            "name": "rule-search SKILL",
            "description": "2-step workflow for querying team rules via MCP: search_rules -> get_rules.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/rule-gap",
            "name": "rule-gap SKILL",
            "description": "3-step recipe for finding review feedback patterns not yet covered by a team rule.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/rule-diff",
            "name": "rule-diff SKILL",
            "description": "Summarize team rule changes since the last `difflore cloud sync` - added, strengthened, removed.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/rule-why-fired",
            "name": "rule-why-fired SKILL",
            "description": "Explain why a specific rule matched the current file or diff (file-pattern / semantic / past-verdict reasons).",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/rule-journey",
            "name": "rule-journey SKILL",
            "description": "Narrative report workflow for the evolution of a team's DiffLore rule library.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/smart-explore",
            "name": "smart-explore SKILL",
            "description": "Cheap repo-map workflow before agents read files or expand rules.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/knowledge-agent",
            "name": "knowledge-agent SKILL",
            "description": "Answer cross-cutting questions over team review memory via `difflore ask` plus MCP rule tools.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/session-recap",
            "name": "session-recap SKILL",
            "description": "Surface DiffLore's per-session value to the user in one line at wrap-up, sourced from `difflore status`.",
            "mimeType": "text/markdown"
        },
        {
            "uri": "difflore://skills/difflore-onboard",
            "name": "difflore-onboard SKILL",
            "description": "Guide a repo from cold checkout to first useful recall: init, import reviews, recall, with receipts.",
            "mimeType": "text/markdown"
        }
    ])
}

/// URI-template resources (MCP `resources/templates/list`). The client fills in
/// the placeholders; the static `resources/list` above does not enumerate every
/// verdict or signature id since the set is unbounded and cloud-scoped.
pub(super) fn resource_templates_list() -> Value {
    json!([
        {
            "uriTemplate": "difflore://verdicts/{id}",
            "name": "Past verdict detail",
            "description": "Full detail of a past review verdict (what the team decided, why). Cite by id in agent replies so the user can click through to the cloud dashboard.",
            "mimeType": "application/json"
        },
        {
            "uriTemplate": "difflore://signatures/{hash}",
            "name": "Fix signature",
            "description": "Canonical fix-signature pointer. Returns the hash echo plus a cloud deep link; signature clustering data lives in the cloud.",
            "mimeType": "application/json"
        }
    ])
}

/// Full trigger guide for the `remember_rule` tool. Kept as a resource so the
/// tool description stays terse (saves ~1.5KiB per initialize) while agents can
/// still pull the full decision tree on demand.
pub(super) const REMEMBER_RULE_GUIDE_MD: &str = r#"# `remember_rule` - Full Trigger Guide

**Persist a coding rule to DiffLore so it applies to future reviews and agent sessions.** This tool is the ONLY way a rule the user mentions in chat actually gets saved - saying "got it, I'll remember" without calling this tool means the rule is lost the moment the conversation ends.

## MUST CALL when the user expresses intent like (in any language):

- "remember this" / "save this rule" / "note this down"
- "don't do X again" / "never do X" / "stop doing X"
- "from now on, X" / "going forward, X" / "whenever you write X, do Y"
- "add a rule that X" / "make a rule for X"
- "in this codebase we always X" / "our convention is X"

The trigger is the user's *intent* to make the rule stick across sessions - phrasing varies by language and tone, so match on intent, not exact words. If you're unsure, prefer calling the tool: a wrong-but-recoverable capture beats a silently dropped rule.

## MUST NOT call when:

- The user is just asking a question or making an observation ("what does X do?")
- You inferred the rule from code review without the user saying so (use `search_rules` then `get_rules` for that flow)
- The user asked you to remember something that's NOT a coding rule (e.g. a meeting time)

## Transcribe verbatim

Put the user's own words in `body` - don't paraphrase or summarise the reasoning away. Their wording often carries the WHY that makes the rule useful. If the user wrote in a non-English language, keep their original wording in `body` (the rule body is opaque text); use English for the `title` so audit listings stay scannable.

## After calling, confirm to the user

Echo the returned `rule_id` and one sentence ("Saved as Rule X - applies to next review of TS files"). The rule is available to local memory immediately.
"#;
