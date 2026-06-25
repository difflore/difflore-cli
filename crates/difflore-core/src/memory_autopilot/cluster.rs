use std::collections::{BTreeMap, HashSet};

use crate::cloud::session_mined::SessionMinedCandidate;
use crate::skills::semantic_dedup::{jaccard, tokenize_knowledge_text, tokenize_semantic_title};

use super::*;

const TITLE_CLUSTER_JACCARD: f32 = 0.70;
const TITLE_BRANCH_CONTENT_FLOOR: f32 = 0.30;
const TITLE_SOFT_CLUSTER_JACCARD: f32 = 0.50;
const TITLE_SOFT_CONTENT_FLOOR: f32 = 0.30;
const CONTENT_CLUSTER_JACCARD: f32 = 0.45;
const PATH_FAMILY_CONTENT_CLUSTER_JACCARD: f32 = 0.23;
const PATH_FAMILY_TITLE_CLUSTER_JACCARD: f32 = 0.25;
const PATH_FAMILY_SOFT_BOOST: f32 = 0.08;

pub fn session_mined_candidates_semantically_match(
    left: &SessionMinedCandidate,
    right: &SessionMinedCandidate,
) -> bool {
    normalize_token(&left.source_repo) == normalize_token(&right.source_repo)
        && CandidateClusterKey::from_parts(&left.title, &left.body, &left.file_patterns).matches(
            &CandidateClusterKey::from_parts(&right.title, &right.body, &right.file_patterns),
        )
}

pub(super) fn group_pending_memories(
    pending: Vec<PendingMemory>,
) -> Vec<(String, Vec<PendingMemory>)> {
    let mut grouped = Vec::new();
    let mut non_session_groups: BTreeMap<String, Vec<PendingMemory>> = BTreeMap::new();
    let mut sessions_by_repo: BTreeMap<String, Vec<PendingMemory>> = BTreeMap::new();

    for candidate in pending {
        if matches!(candidate.kind, PendingMemoryKind::Session { .. }) {
            sessions_by_repo
                .entry(cluster_repo_key(&candidate))
                .or_default()
                .push(candidate);
        } else {
            non_session_groups
                .entry(candidate_group_key(&candidate))
                .or_default()
                .push(candidate);
        }
    }

    grouped.extend(non_session_groups);
    for (_, mut candidates) in sessions_by_repo {
        candidates.sort_by_key(candidate_sort_key);
        for mut cluster in cluster_session_candidates(candidates) {
            cluster.sort_by_key(candidate_sort_key);
            let key = cluster_group_key(&cluster);
            grouped.push((key, cluster));
        }
    }

    grouped.sort_by(|(left_key, left), (right_key, right)| {
        left_key
            .cmp(right_key)
            .then_with(|| first_item_id(left).cmp(first_item_id(right)))
    });
    grouped
}

fn cluster_session_candidates(candidates: Vec<PendingMemory>) -> Vec<Vec<PendingMemory>> {
    if candidates.len() <= 1 {
        return candidates
            .into_iter()
            .map(|candidate| vec![candidate])
            .collect();
    }

    let mut union_find = UnionFind::new(candidates.len());
    let keys = candidates
        .iter()
        .map(CandidateClusterKey::from_candidate)
        .collect::<Vec<_>>();
    for left in 0..keys.len() {
        for right in (left + 1)..keys.len() {
            if keys[left].matches(&keys[right]) {
                union_find.union(left, right);
            }
        }
    }

    let mut by_root: BTreeMap<usize, Vec<PendingMemory>> = BTreeMap::new();
    for (idx, candidate) in candidates.into_iter().enumerate() {
        by_root
            .entry(union_find.find(idx))
            .or_default()
            .push(candidate);
    }
    by_root.into_values().collect()
}

fn cluster_repo_key(candidate: &PendingMemory) -> String {
    candidate
        .source_repo
        .as_deref()
        .map(normalize_token)
        .filter(|repo| !repo.is_empty())
        .unwrap_or_else(|| "any-repo".to_owned())
}

fn cluster_group_key(candidates: &[PendingMemory]) -> String {
    choose_cluster_representative(candidates)
        .map_or_else(|| "empty-cluster".to_owned(), candidate_group_key)
}

fn choose_cluster_representative(candidates: &[PendingMemory]) -> Option<&PendingMemory> {
    candidates.iter().max_by(|left, right| {
        left.body
            .chars()
            .count()
            .cmp(&right.body.chars().count())
            .then_with(|| left.title.len().cmp(&right.title.len()))
            .then_with(|| right.item_id.cmp(&left.item_id))
    })
}

fn candidate_sort_key(candidate: &PendingMemory) -> (String, String) {
    (candidate_group_key(candidate), candidate.item_id.clone())
}

fn first_item_id(candidates: &[PendingMemory]) -> &str {
    candidates
        .first()
        .map_or("", |candidate| candidate.item_id.as_str())
}

#[derive(Debug)]
struct CandidateClusterKey {
    title_tokens: HashSet<String>,
    content_tokens: HashSet<String>,
    path_families: HashSet<String>,
}

impl CandidateClusterKey {
    fn from_candidate(candidate: &PendingMemory) -> Self {
        Self::from_parts(&candidate.title, &candidate.body, &candidate.file_patterns)
    }

    fn from_parts(title: &str, body: &str, file_patterns: &[String]) -> Self {
        Self {
            title_tokens: tokenize_semantic_title(title),
            content_tokens: tokenize_knowledge_text(body),
            path_families: path_families(file_patterns),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        let title_score = jaccard(&self.title_tokens, &other.title_tokens);
        let content_score = jaccard(&self.content_tokens, &other.content_tokens);
        let boost = if path_family_overlap(&self.path_families, &other.path_families) {
            PATH_FAMILY_SOFT_BOOST
        } else {
            0.0
        };
        content_score + boost >= CONTENT_CLUSTER_JACCARD
            || (title_score >= TITLE_CLUSTER_JACCARD && content_score >= TITLE_BRANCH_CONTENT_FLOOR)
            || (title_score >= TITLE_SOFT_CLUSTER_JACCARD
                && content_score >= TITLE_SOFT_CONTENT_FLOOR)
            || (boost > 0.0
                && title_score >= PATH_FAMILY_TITLE_CLUSTER_JACCARD
                && content_score >= PATH_FAMILY_CONTENT_CLUSTER_JACCARD)
    }
}

fn path_families(patterns: &[String]) -> HashSet<String> {
    patterns
        .iter()
        .flat_map(|pattern| {
            pattern
                .split(['/', '*', '{', '}', '.', '_', '-'])
                .map(str::trim)
                .filter(|part| part.len() >= 3)
                .filter(|part| {
                    !matches!(
                        *part,
                        "app"
                            | "apps"
                            | "component"
                            | "components"
                            | "crate"
                            | "crates"
                            | "css"
                            | "json"
                            | "jsx"
                            | "less"
                            | "lib"
                            | "libs"
                            | "module"
                            | "modules"
                            | "package"
                            | "scss"
                            | "src"
                            | "test"
                            | "tests"
                            | "tsx"
                    )
                })
                .map(normalize_token)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn path_family_overlap(left: &HashSet<String>, right: &HashSet<String>) -> bool {
    !left.is_empty() && !right.is_empty() && left.iter().any(|family| right.contains(family))
}

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, idx: usize) -> usize {
        let parent = self.parent[idx];
        if parent == idx {
            idx
        } else {
            let root = self.find(parent);
            self.parent[idx] = root;
            root
        }
    }

    fn union(&mut self, left: usize, right: usize) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root != right_root {
            self.parent[right_root] = left_root;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_candidate(
        idx: usize,
        title: &str,
        body: &str,
        patterns: Vec<&str>,
    ) -> PendingMemory {
        PendingMemory {
            item_id: format!("session:hash-{idx}"),
            kind: PendingMemoryKind::Session {
                content_hash: format!("hash-{idx}"),
            },
            title: title.to_owned(),
            body: body.to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "session_mined".to_owned(),
            source_repo: Some("hizachlee/cortex".to_owned()),
            file_patterns: patterns.into_iter().map(str::to_owned).collect(),
            verdict: Some("KEEP".to_owned()),
            session_id: Some(format!("session-{idx}")),
            session_created_at_ms: Some(1_714_000_000_000 + idx as i64),
            distinct_evidence_count: None,
            autopilot_disabled: false,
        }
    }

    fn group_signatures(groups: Vec<(String, Vec<PendingMemory>)>) -> Vec<Vec<String>> {
        groups
            .into_iter()
            .map(|(_, candidates)| {
                candidates
                    .into_iter()
                    .map(|candidate| candidate.item_id)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn clusters_real_tauri_dev_variants_with_empty_or_divergent_patterns() {
        let candidates = vec![
            session_candidate(
                1,
                "Tauri dev startup: npm run tauri dev, not raw binary",
                "For Cortex desktop local development, launch with npm run tauri dev so Vite \
                 starts on localhost 1420 and the Tauri shell can load frontend assets. Do not \
                 run the compiled binary alone for debug testing because it can show an empty UI.",
                vec![],
            ),
            session_candidate(
                2,
                "Cortex Desktop Dev Startup: Use npm run tauri dev, not binary",
                "When launching the cortex-desktop app in debug mode, use npm run tauri dev from \
                 the desktop crate. That command starts both the frontend dev server and the Tauri \
                 wrapper; opening the compiled binary by itself leaves the webview without assets.",
                vec!["crates/cortex-desktop/package.json"],
            ),
            session_candidate(
                3,
                "Tauri+Vite dev launch: always use npm run tauri dev, not binary alone",
                "Local Tauri app testing needs the Vite dev server and desktop shell running \
                 together. Start from crates/cortex-desktop with npm run tauri dev rather than \
                 executing only the built app binary.",
                vec!["crates/cortex-desktop/src-tauri/**"],
            ),
            session_candidate(
                4,
                "Cortex Desktop: Use npm run tauri dev for local dev, not binary alone",
                "Debug builds of the Cortex desktop app load UI resources from Vite instead of \
                 embedding them. Prefer npm run tauri dev for local startup so the frontend server \
                 exists before the shell tries to render.",
                vec![],
            ),
            session_candidate(
                5,
                "Tauri desktop app dev launch: Vite + local server",
                "For desktop development, treat Vite and Tauri as one launch path. The local \
                 server must be up for the debug shell, so npm run tauri dev is the safe startup \
                 command and the raw binary is not.",
                vec!["crates/cortex-desktop/src-tauri/tauri.conf.json"],
            ),
            session_candidate(
                6,
                "Cortex Desktop Debug Startup: Tauri + Vite Dev Server",
                "A blank desktop window in debug usually means the shell was started without the \
                 Vite dev server. Use the Tauri dev command from the cortex desktop package so \
                 both sides come up together.",
                vec!["crates/cortex-desktop/package.json"],
            ),
        ];

        let groups = group_pending_memories(candidates);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 6);
    }

    #[test]
    fn clustering_is_deterministic_for_reordered_input() {
        let body = "When preparing Mac App Store builds, avoid private macOS vibrancy APIs and \
                    implement glass effects inside the webview with CSS backdrop filters.";
        let candidates = vec![
            session_candidate(
                1,
                "Avoid native macOS vibrancy/private APIs for Mac App Store builds",
                body,
                vec!["src-tauri/**/*.rs"],
            ),
            session_candidate(
                2,
                "Mac App Store release: avoid Tauri private vibrancy APIs",
                body,
                vec![],
            ),
            session_candidate(
                3,
                "Keep Mac App Store builds free of private macOS vibrancy APIs",
                body,
                vec!["src/**/*.css"],
            ),
        ];
        let mut reversed = candidates.clone();
        reversed.reverse();

        assert_eq!(
            group_signatures(group_pending_memories(candidates)),
            group_signatures(group_pending_memories(reversed))
        );
    }

    #[test]
    fn similar_titles_with_divergent_bodies_stay_separate() {
        let candidates = vec![
            session_candidate(
                1,
                "ExternalLink navigation for cross deployment routes",
                "Use ExternalLink when navigating to pages hosted outside this deployment so the browser performs a full document navigation.",
                vec!["src/modules/ExternalLink.tsx"],
            ),
            session_candidate(
                2,
                "ExternalLink navigation for internal router routes",
                "Do not use ExternalLink for internal TanStack router destinations; keep those navigations inside the client router.",
                vec!["src/routes/**/*.tsx"],
            ),
        ];

        let groups = group_pending_memories(candidates);

        assert_eq!(groups.len(), 2);
    }
}
