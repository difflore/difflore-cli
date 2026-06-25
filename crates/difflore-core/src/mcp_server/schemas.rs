use serde_json::{Value, json};

use super::tools::CONTROL_PLANE_DENIED_TOOL_NAMES;

pub(super) use super::skill_docs::{
    DIFFLORE_ONBOARD_SKILL_MD, KNOWLEDGE_AGENT_SKILL_MD, MEMORY_CANDIDATE_TRIAGE_SKILL_MD,
    PRE_SUBMIT_REVIEW_SKILL_MD, RULE_DIFF_SKILL_MD, RULE_GAP_SKILL_MD, RULE_JOURNEY_SKILL_MD,
    RULE_SEARCH_SKILL_MD, RULE_WHY_FIRED_SKILL_MD, SESSION_RECAP_SKILL_MD, SMART_EXPLORE_SKILL_MD,
};

pub(super) fn tools_list() -> Value {
    json!([
        {
            "name": "search_rules",
            "description": "Compact memory search. Returns rule ids/titles/origins plus match reasons before fetching details. Memory is scoped to the current git remotes; pass `repo_full_name` (repo namespace path such as GitHub owner/repo or GitLab group/project) when auto-detection is unavailable. Results are deterministically ordered by relative-score band, then path hint, then source priority manual > team > pr_review > extracted > conversation, and each carries a compact `why` ranking explanation (e.g. `path-hint; band 9/10; source manual`). When team review history is available, results include citedCount and trustRate so agents can prefer rules that led to accepted edits. Use with get_rules to expand only matched rules.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Current file path (adds a path-hint ranking boost)"
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
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; retrieve and cite approved active rules only; use CLI commands for memory mutations",
                "deniedMutations": CONTROL_PLANE_DENIED_TOOL_NAMES
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
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; fetch approved active rule details only; use CLI commands for memory mutations"
            }
        },
        {
            "name": "get_past_verdicts",
            "description": "Search team review history (WHAT the team decided on similar code before). Cloud-backed reads are disabled by default for MCP and require explicit local opt-in (`DIFFLORE_MCP_ALLOW_CLOUD_READS=1`). Memory is scoped to the current repo/project only; pass `repo_full_name` (repo namespace path such as GitHub owner/repo or GitLab group/project) when auto-detection is unavailable. Pass `file` (the path you're editing) so DiffLore can prioritize matching file patterns and show useful gaps for that file. Use this to cite concrete prior decisions; use `rule_timeline` when you need the *why this rule exists* narrative for a specific rule.",
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
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; cloud-backed read requires explicit local opt-in; use CLI for login/sync/publish"
            }
        },
        {
            "name": "list_memory",
            "description": "Read DiffLore memory across lifecycle states: active rules, pending local drafts, and session-mined candidates. Use when the user asks what DiffLore learned, which candidates exist, or which memories are active. This is AI-readable inventory only; do not approve, reject, sync, archive, or delete memory through MCP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["active", "draft", "candidate", "pending", "all"],
                        "description": "Optional lifecycle state filter. `pending` means drafts plus candidates."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["rule", "draft", "candidate", "pending", "all"],
                        "description": "Optional item kind filter."
                    },
                    "repo_full_name": {
                        "type": "string",
                        "description": "Optional repo scope, e.g. GitHub owner/repo or self-managed GitLab host/group/project."
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional text filter over id, title, summary, and file patterns."
                    },
                    "limit": {
                        "type": "number",
                        "default": 50,
                        "minimum": 1,
                        "maximum": 1000,
                        "description": "Maximum number of memory items to return."
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; inventory only; use CLI commands for approve, reject, disable, sync, archive, or delete memory"
            }
        },
        {
            "name": "get_memory_item",
            "description": "Read one DiffLore memory item by id, including full body and provenance where available. Accepts `rule:<id>`, `draft:<id>`, or `session:<content_hash>`. Use this before advising a user to approve or reject a candidate. This tool does not mutate memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Memory item id, such as `rule:conv-x`, `draft:conv-x`, or `session:abc123...`."
                    }
                },
                "required": ["id"]
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; inspect only; use CLI commands for approve, reject, disable, sync, archive, or delete memory"
            }
        },
        {
            "name": "get_memory_activity",
            "description": "Read local evidence that active rules were retrieved or surfaced to agents. Activity is not proof that a rule changed the final code; describe it as surfaced/retrieved unless stronger outcome proof exists elsewhere.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rule_id": {
                        "type": "string",
                        "description": "Optional active rule id to filter activity."
                    },
                    "repo_full_name": {
                        "type": "string",
                        "description": "Optional repo scope filter."
                    },
                    "days": {
                        "type": "number",
                        "default": 30,
                        "minimum": 1,
                        "maximum": 365
                    },
                    "limit": {
                        "type": "number",
                        "default": 20,
                        "minimum": 1,
                        "maximum": 1000
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; activity is retrieval/surface evidence, not proof of code influence"
            }
        },
        {
            "name": "get_memory_digest",
            "description": "Read the Memory Autopilot digest plus background schedule/status: enabled memories, items needing review, muted duplicates, conservative reasons, dirty/run counters, and the last background result. This MCP tool is read-only for AI; explain the digest and ask the user to run `difflore memory review`, `difflore memory inbox`, `difflore memory log`, or `difflore status` for normal follow-up. Background Memory Autopilot runs automatically; explicit `difflore memory autopilot` is for manual catch-up and debugging only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "number",
                        "default": 20,
                        "minimum": 1,
                        "maximum": 1000,
                        "description": "Maximum candidate groups to include in the digest."
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; use CLI commands for review, inbox, status, disable, approve, reject, sync, archive, or delete memory"
            }
        },
        {
            "name": "get_memory_autopilot_log",
            "description": "Read the local Memory Autopilot audit log. Use this to explain what Autopilot did and why; do not approve, disable, reject, sync, archive, or delete memory through MCP. Ask the user to run the DiffLore CLI for any mutation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "number",
                        "default": 20,
                        "minimum": 1,
                        "maximum": 1000,
                        "description": "Maximum audit events to return."
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; use CLI commands for review, disable, approve, reject, sync, archive, delete, or manual catch-up/debug memory actions"
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
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; timeline evidence only; use CLI commands for memory mutations"
            }
        },
        {
            "name": "remember_rule",
            "description": "Save and activate a local DiffLore memory rule from a coding rule the user explicitly asked to remember. A direct user \"remember this\" request is treated as approval, so fresh captures are active and served to agents immediately. Call WHENEVER the user signals intent to make a rule stick (\"remember this\", \"from now on\", \"don't do X again\", \"always require tests before merge\", \"make this a rule\"). Pass `title` as a short imperative and `body` containing the user's reasoning in English - the WHY, not just what (translate and summarise it if they explained in another language). Return the active rule id and tell the user it has been saved and enabled. Full trigger guide at difflore://skills/remember_rule.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short imperative title (80 chars or fewer). E.g. \"Avoid Promise.race for timeout in fetch wrappers\"."
                    },
                    "body": {
                        "type": "string",
                        "description": "Full natural-language explanation in English. Include the WHY, not just the what - summarise the user's reasoning in English (translate it if they explained in another language). Multi-paragraph OK."
                    },
                    "file_patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional path hints/evidence globs for the rule (e.g. [\"**/*.ts\", \"src/api/**\"]). Omit for repo-wide rules. These boost ranking on matching files but do not hard-filter recall."
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
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["review_rule", "soft_preference"],
                        "description": "Defaults to review_rule. Use soft_preference only for lightweight team/user context or workflow preferences that should be always visible, not precision review rules."
                    },
                    "category": {
                        "type": "string",
                        "enum": ["workflow_preference", "user_preference", "project_context"],
                        "description": "Optional soft_preference category."
                    }
                },
                "required": ["title", "body"]
            },
            "_meta": {
                "governance": "user_requested_active_rule; explicit remember request counts as local approval; use CLI commands for disable/sync/publish"
            }
        },
        {
            "name": "plan_pr",
            "description": "Read-only planning aid before editing: given an issue/PR description (`intent`), returns the expected file count, file-category mix, and the closest historical PRs from local review history. It does not mutate files, memory, cloud state, or PRs. Use this to avoid silently under-completing - when the team's prior pattern for similar work touches 4+ files, finishing at 2 is the failure mode this prevents. Falls back to an empty prediction with a hint when no local PR review data exists - run `difflore import-reviews` to populate.",
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
            },
            "annotations": {
                "readOnlyHint": true
            },
            "_meta": {
                "governance": "read_only_for_ai; planning only; no file, memory, cloud, or PR mutation"
            }
        }
    ])
}

/// One static markdown skill resource: its `difflore://skills/...` URI, the
/// `resources/list` display name + description, and the markdown body served by
/// `resources/read`. Single source of truth so the list advertisement and the
/// read handler can never drift (every skill has both, byte-identical).
pub(super) struct SkillResource {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub markdown: &'static str,
}

/// All static markdown skill resources. `resources_list()` advertises these
/// (plus the two dynamic resources) and `handle_resources_read` serves their
/// markdown by URI lookup.
pub(super) const SKILL_RESOURCES: &[SkillResource] = &[
    SkillResource {
        uri: "difflore://skills/remember_rule",
        name: "remember_rule trigger guide",
        description: "Full guide for when to call the remember_rule MCP tool, with trigger phrases and anti-patterns.",
        markdown: REMEMBER_RULE_GUIDE_MD,
    },
    SkillResource {
        uri: "difflore://skills/rule-search",
        name: "rule-search SKILL",
        description: "2-step workflow for querying team rules via MCP: search_rules -> get_rules.",
        markdown: RULE_SEARCH_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/rule-gap",
        name: "rule-gap SKILL",
        description: "3-step recipe for finding review feedback patterns not yet covered by a team rule.",
        markdown: RULE_GAP_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/rule-diff",
        name: "rule-diff SKILL",
        description: "Summarize team rule changes since the last `difflore cloud sync` - added, strengthened, removed.",
        markdown: RULE_DIFF_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/rule-why-fired",
        name: "rule-why-fired SKILL",
        description: "Explain why a specific rule matched the current file or diff (file-pattern / semantic / past-verdict reasons).",
        markdown: RULE_WHY_FIRED_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/rule-journey",
        name: "rule-journey SKILL",
        description: "Narrative report workflow for the evolution of a team's DiffLore rule library.",
        markdown: RULE_JOURNEY_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/smart-explore",
        name: "smart-explore SKILL",
        description: "Cheap repo-map workflow before agents read files or expand rules.",
        markdown: SMART_EXPLORE_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/knowledge-agent",
        name: "knowledge-agent SKILL",
        description: "Answer cross-cutting questions over team review memory via `difflore ask` plus MCP rule tools.",
        markdown: KNOWLEDGE_AGENT_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/memory-candidate-triage",
        name: "memory-candidate-triage SKILL",
        description: "Inspect and group pending DiffLore memory candidates without approving or rejecting them through MCP.",
        markdown: MEMORY_CANDIDATE_TRIAGE_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/session-recap",
        name: "session-recap SKILL",
        description: "Surface concrete difflore accepted-edit activity in one quiet wrap-up line when this task captured it.",
        markdown: SESSION_RECAP_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/pre-submit-review",
        name: "pre-submit-review SKILL",
        description: "Run a local DiffLore review before commit, push, PR, or handoff, then fix and re-check.",
        markdown: PRE_SUBMIT_REVIEW_SKILL_MD,
    },
    SkillResource {
        uri: "difflore://skills/difflore-onboard",
        name: "difflore-onboard SKILL",
        description: "Guide a repo from cold checkout to first useful recall: init, import reviews, recall, with receipts.",
        markdown: DIFFLORE_ONBOARD_SKILL_MD,
    },
];

pub(super) fn resources_list() -> Value {
    let mut resources = vec![
        json!({
            "uri": "difflore://rules/active",
            "name": "Active Rules",
            "description": "All active rules for the current project, formatted as Markdown",
            "mimeType": "text/markdown"
        }),
        json!({
            "uri": "difflore://memory/inbox",
            "name": "Memory Inbox",
            "description": "Structured local memory inventory: active rules, pending drafts, session-mined candidates, queues, and local activity counts.",
            "mimeType": "application/json"
        }),
    ];
    resources.extend(SKILL_RESOURCES.iter().map(|r| {
        json!({
            "uri": r.uri,
            "name": r.name,
            "description": r.description,
            "mimeType": "text/markdown"
        })
    }));
    Value::Array(resources)
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

**Save and activate a local DiffLore memory rule from a coding rule the user explicitly asked to remember.** A direct user "remember this" request counts as approval, so fresh captures are active and served to agents immediately. This tool is the durable capture path; saying "got it, I'll remember" without calling it means the rule is lost the moment the conversation ends.

## MUST CALL when the user expresses intent like (in any language):

- "remember this" / "save this rule" / "note this down"
- "don't do X again" / "never do X" / "stop doing X"
- "from now on, X" / "going forward, X" / "whenever you write X, do Y"
- "add a rule that X" / "make a rule for X"
- "in this codebase we always X" / "our convention is X"
- "next time use X" / "I prefer X in this repo" when it changes future coding behavior

The trigger is the user's *intent* to make the rule stick across sessions - phrasing varies by language and tone, so match on intent, not exact words. If you're unsure, ask one short confirmation first.

## Call shape

```text
remember_rule(
  title="short actionable rule",
  body="fuller explanation with context",
  file_patterns=["src/**/*.rs"],     # narrow when file/subsystem-specific
  severity="low|medium|high",
  bad_code="optional: what to avoid",
  good_code="optional: preferred form",
)
```

## MUST NOT call when:

- The user is just asking a question or making an observation ("what does X do?")
- The user offered a one-off correction or preference for the current task but did not signal future use
- The preference is a one-off taste note, not durable coding guidance
- You inferred the rule from code review without the user saying so (use `search_rules` then `get_rules` for that flow)
- The user asked you to remember something that's NOT a coding rule (e.g. a meeting time)
- The rule is vague, non-actionable, or broad project history rather than coding guidance
- The content contains secrets or private chat content
- An existing rule already covers it - search first if unsure

## Capture the WHY in English

Capture the user's reasoning in `body` - the WHY, not just the what; the reasoning is what makes the rule useful. Write both `title` and `body` in English: if the user explained in another language, translate and summarise their point into clear English rather than pasting the original wording. Stay faithful to their meaning - don't drop the substance of the reason.

## After calling, confirm to the user

Echo the returned `item_id`, explain that it is active because the user explicitly asked to remember it, and show the CLI command to inspect it:

```bash
difflore memory show rule:<id>
```

If the `remember_rule` MCP tool is not available after tool discovery, use the CLI fallback instead:

```bash
difflore memory remember --title "<short actionable rule>" --body "<full context>" --file-pattern "src/**/*.rs" --json
```

The CLI fallback also treats the explicit user request as approval and saves an active rule. Tell the user you saved and enabled it.

If the tool says it strengthened an existing active rule, say that rule remains available to agents.

Do not reject, sync, archive, delete, or edit other memory through MCP. To undo an active remembered rule, tell the user they can run:

```bash
difflore memory disable rule:<id>
```
"#;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn normalized_markdown(text: &str) -> String {
        text.replace("\r\n", "\n")
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    }

    fn markdown_body_without_frontmatter(text: &str) -> &str {
        let body = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n").map(|(_, body)| body));
        let body = body.unwrap_or_else(|| panic!("expected skill markdown frontmatter"));
        body.strip_prefix('\n').unwrap_or(body)
    }

    #[test]
    fn remember_rule_guide_matches_plugin_skill_body() {
        let skills_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin/skills");
        let path = skills_root.join("remember-rule-guide").join("SKILL.md");
        if !path.exists() {
            return;
        }

        let plugin_doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
        let plugin_doc = normalized_markdown(&plugin_doc);
        let plugin_body = markdown_body_without_frontmatter(&plugin_doc);
        assert_eq!(
            normalized_markdown(REMEMBER_RULE_GUIDE_MD),
            plugin_body,
            "{} drifted from the MCP remember_rule guide",
            path.display()
        );
    }
}
