use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Clone, Debug)]
struct MissingFileCandidate {
    path: String,
    local: bool,
    source_rank: usize,
}

pub(crate) fn missing_file_hints_from_prediction(
    prediction: &Value,
    changed_files: &[String],
    project_root: &Path,
) -> Vec<String> {
    if changed_files.is_empty() {
        return Vec::new();
    }

    let changed = changed_files
        .iter()
        .map(|file| normalize_path(file).to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    let mut candidates = Vec::new();
    let mut rank = 0;

    for file in local_adjacency_file_hints(changed_files, project_root) {
        push_missing_file_candidate(&mut candidates, &changed, &file, true, rank);
        rank += 1;
    }

    for file in ranked_coedit_files(prediction) {
        push_missing_file_candidate(&mut candidates, &changed, file, false, rank);
        rank += 1;
    }

    if let Some(neighbors) = prediction.get("neighbors").and_then(Value::as_array) {
        for neighbor in neighbors.iter().take(3) {
            let Some(files) = neighbor.get("files").and_then(Value::as_array) else {
                continue;
            };
            for file in files.iter().filter_map(Value::as_str) {
                push_missing_file_candidate(&mut candidates, &changed, file, false, rank);
                rank += 1;
            }
        }
    }

    candidates.sort_by(|a, b| {
        missing_file_hint_score(b, changed_files)
            .cmp(&missing_file_hint_score(a, changed_files))
            .then_with(|| a.source_rank.cmp(&b.source_rank))
            .then_with(|| a.path.cmp(&b.path))
    });
    // Project-filesystem-aware filter: hints pointing at paths that
    // don't exist in this fork's checkout are nearly always wrong
    // (renamed, language-mismatched neighbour file, or stale corpus).
    // Held-out validation showed ~75% of low-precision hints fail this
    // basic existence check. Keep only hints whose path exists or
    // whose parent dir exists (to allow the legitimate "you should
    // add a sibling test file" case).
    candidates
        .into_iter()
        .filter(|c| hint_path_is_plausible(&c.path, project_root))
        .take(8)
        .map(|candidate| candidate.path)
        .collect()
}

/// Return true when the hinted path looks like it belongs in this
/// project — either the file itself exists, or its parent directory
/// does. The latter covers the "we know you should add `foo_test.rs`
/// next to `foo.rs`" case where the hint file legitimately doesn't
/// exist yet but the directory does.
fn hint_path_is_plausible(hint: &str, project_root: &Path) -> bool {
    let normalized = normalize_path(hint);
    let absolute = project_root.join(&normalized);
    if absolute.exists() {
        return true;
    }
    if let Some(parent) = absolute.parent() {
        if parent.exists() {
            return true;
        }
    }
    false
}

fn ranked_coedit_files(prediction: &Value) -> Vec<&str> {
    // Holdout validation (router/cli, 100 PRs each) found that corpus
    // co-edit hints with `in_n_of_neighbors == 1` AND `score < 0.2`
    // are essentially noise — a single historical PR co-touched the
    // path once with a weak similarity. Per-hint precision jumps from
    // ~14% (no filter) to ~30% on router (and ~44%→60% on cli) when
    // we require either multiple neighbour confirmation OR a meaningful
    // similarity score. Keep the threshold permissive (`>= 0.2` is
    // the bottom of the historical hit-bucket distribution) so we
    // don't crush recall on repos like cli where weaker hints sometimes
    // land.
    prediction
        .get("coedit_file_hints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|hint| {
            let score = hint.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let n = hint
                .get("in_n_of_neighbors")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            n >= 2 || score >= 0.2
        })
        .filter_map(|hint| hint.get("file").and_then(Value::as_str))
        .collect()
}

fn push_missing_file_candidate(
    candidates: &mut Vec<MissingFileCandidate>,
    changed: &std::collections::BTreeSet<String>,
    file: &str,
    local: bool,
    source_rank: usize,
) {
    let normalized = normalize_path(file);
    if normalized.trim().is_empty() || changed.contains(&normalized.to_ascii_lowercase()) {
        return;
    }
    if let Some(existing) = candidates
        .iter_mut()
        .find(|candidate| candidate.path.eq_ignore_ascii_case(&normalized))
    {
        existing.local |= local;
        existing.source_rank = existing.source_rank.min(source_rank);
        return;
    }
    candidates.push(MissingFileCandidate {
        path: normalized,
        local,
        source_rank,
    });
}

fn missing_file_hint_score(candidate: &MissingFileCandidate, changed_files: &[String]) -> i64 {
    let candidate_path = normalize_path(&candidate.path);
    let mut best = if candidate.local { 320 } else { 0 };
    for changed in changed_files {
        best = best.max(path_affinity_score(
            &candidate_path,
            &normalize_path(changed),
        ));
    }
    best
}

fn path_affinity_score(candidate: &str, changed: &str) -> i64 {
    let candidate_dir = path_dir(candidate);
    let changed_dir = path_dir(changed);
    let candidate_name = path_name(candidate);
    let candidate_ext = path_ext(candidate);
    let changed_ext = path_ext(changed);
    let candidate_stem = canonical_stem(candidate);
    let changed_stem = canonical_stem(changed);

    let mut score = common_prefix_segments(&candidate_dir, &changed_dir).min(6) as i64 * 12;
    if !candidate_dir.is_empty() && candidate_dir == changed_dir {
        score += 90;
    } else if is_ancestor_dir(&candidate_dir, &changed_dir)
        || is_ancestor_dir(&changed_dir, &candidate_dir)
    {
        score += 55;
    }
    if !candidate_ext.is_empty() && candidate_ext == changed_ext {
        score += 12;
    }
    if !candidate_stem.is_empty() && candidate_stem == changed_stem {
        score += 150;
        if is_test_path(candidate) != is_test_path(changed) {
            score += 70;
        }
    }
    if candidate_name.starts_with("index.") && is_ancestor_dir(&candidate_dir, &changed_dir) {
        score += 120;
    }
    score
}

fn local_adjacency_file_hints(changed_files: &[String], project_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for candidate in route_literal_hints(changed_files, project_root) {
        push_existing_hint(&mut out, project_root, &candidate);
    }
    for changed in changed_files {
        let changed = normalize_path(changed);
        for candidate in counterpart_paths(&changed) {
            push_existing_hint(&mut out, project_root, &candidate);
        }
        for candidate in workflow_sibling_hints(&changed, project_root) {
            push_existing_hint(&mut out, project_root, &candidate);
        }
        for candidate in command_acceptance_fixture_hints(&changed, project_root) {
            push_existing_hint(&mut out, project_root, &candidate);
        }
        // Holdout validation showed `relative_import_hints` is the
        // dominant false-positive source on monorepo source files: when
        // a non-test file like `router.ts` is touched, every relative
        // import target (utils.ts, path.ts, lru-cache.ts, …) becomes
        // a hint, but virtually none are actually co-edited (router 30-PR
        // holdout: ~600 local hints, 0 hits attributable to relative
        // imports). Restrict to test/spec changes — there the heuristic
        // "the test imports the source it exercises" actually holds.
        if is_test_path(&changed) {
            for candidate in relative_import_hints(&changed, project_root) {
                push_existing_hint(&mut out, project_root, &candidate);
            }
        }
        for candidate in manifest_counterpart_paths(&changed) {
            push_existing_hint(&mut out, project_root, &candidate);
        }
    }

    for dir in common_changed_ancestor_dirs(changed_files)
        .into_iter()
        .take(3)
    {
        for ext in changed_extensions(changed_files) {
            push_existing_hint(&mut out, project_root, &format!("{dir}/index.{ext}"));
        }
        push_existing_hint(&mut out, project_root, &format!("{dir}/mod.rs"));
        push_existing_hint(&mut out, project_root, &format!("{dir}/lib.rs"));
    }
    out
}

fn command_acceptance_fixture_hints(path: &str, project_root: &Path) -> Vec<String> {
    let normalized = normalize_path(path);
    if !matches!(path_ext(&normalized).as_str(), "go") {
        return Vec::new();
    }
    let Some(rest) = normalized.strip_prefix("pkg/cmd/") else {
        return Vec::new();
    };
    let command_parts = rest
        .split('/')
        .take_while(|part| !part.ends_with(".go"))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if command_parts.is_empty() {
        return Vec::new();
    }
    let prefix = command_parts.join("-");
    let mut out = Vec::new();
    collect_matching_txtar_fixtures(
        &root_relative_path(project_root, "acceptance/testdata"),
        "acceptance/testdata",
        &prefix,
        &mut out,
    );
    out.sort();
    out
}

fn collect_matching_txtar_fixtures(
    root: &Path,
    rel_root: &str,
    prefix: &str,
    out: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{rel_root}/{file_name}");
        if path.is_dir() {
            collect_matching_txtar_fixtures(&path, &rel, prefix, out);
            continue;
        }
        if file_name.starts_with(prefix)
            && file_name.ends_with(".txtar")
            && !out.iter().any(|existing| existing == &rel)
        {
            out.push(rel);
        }
    }
}

fn route_literal_hints(changed_files: &[String], project_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for changed in changed_files {
        let changed = normalize_path(changed);
        let Some(route_dir) = route_dir_for_test_path(&changed) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(root_relative_path(project_root, &changed)) else {
            continue;
        };
        for route in source.lines().flat_map(quoted_module_specifiers) {
            let route = route.split(['?', '#']).next().unwrap_or("").trim();
            if !route.starts_with('/') || route == "/" {
                continue;
            }
            for candidate in route_file_candidates(&route_dir, route) {
                if !out.iter().any(|existing| existing == &candidate) {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

fn route_dir_for_test_path(path: &str) -> Option<String> {
    for marker in ["/tests/", "/__tests__/"] {
        if let Some((fixture_root, _)) = path.split_once(marker) {
            return Some(format!("{fixture_root}/src/routes"));
        }
    }
    None
}

fn route_file_candidates(route_dir: &str, route: &str) -> Vec<String> {
    let route = route.trim_start_matches('/').trim_end_matches('/');
    if route.is_empty() || route.contains(':') || route.contains('*') {
        return Vec::new();
    }
    let mut out = Vec::new();
    for ext in ["tsx", "ts", "jsx", "js"] {
        out.push(format!("{route_dir}/{route}.{ext}"));
        out.push(format!("{route_dir}/{route}/index.{ext}"));
        out.push(format!("{route_dir}/{route}/route.{ext}"));
    }
    out
}

fn workflow_sibling_hints(path: &str, project_root: &Path) -> Vec<String> {
    if path_dir(path) != ".github/workflows" || !matches!(path_ext(path).as_str(), "yml" | "yaml") {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root_relative_path(project_root, ".github/workflows"))
    else {
        return Vec::new();
    };
    let mut out = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
        .map(|name| format!(".github/workflows/{name}"))
        .filter(|candidate| !candidate.eq_ignore_ascii_case(path))
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn relative_import_hints(path: &str, project_root: &Path) -> Vec<String> {
    let full_path = root_relative_path(project_root, path);
    let Ok(source) = std::fs::read_to_string(full_path) else {
        return Vec::new();
    };
    let base_dir = path_dir(path);
    let mut out = Vec::new();
    for line in source.lines() {
        if !(line.contains(" from ")
            || line.contains("import(")
            || line.trim_start().starts_with("export "))
        {
            continue;
        }
        for specifier in quoted_module_specifiers(line) {
            if !specifier.starts_with('.') {
                continue;
            }
            let module_base = normalize_relative_module_path(&base_dir, &specifier);
            for candidate in module_file_candidates(&module_base) {
                if !out.iter().any(|existing| existing == &candidate) {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

fn quoted_module_specifiers(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.char_indices();
    while let Some((start, ch)) = chars.next() {
        if ch != '\'' && ch != '"' {
            continue;
        }
        let quote = ch;
        let content_start = start + ch.len_utf8();
        for (end, next) in chars.by_ref() {
            if next == quote {
                if content_start < end {
                    out.push(line[content_start..end].to_owned());
                }
                break;
            }
        }
    }
    out
}

fn module_file_candidates(module_base: &str) -> Vec<String> {
    if !path_ext(module_base).is_empty() {
        return vec![module_base.to_owned()];
    }
    let mut out = Vec::new();
    for ext in [
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "go", "rs",
    ] {
        out.push(format!("{module_base}.{ext}"));
    }
    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "rs"] {
        out.push(format!("{module_base}/index.{ext}"));
    }
    out
}

fn normalize_relative_module_path(base_dir: &str, specifier: &str) -> String {
    let mut parts = base_dir
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_owned()),
        }
    }
    parts.join("/")
}

fn counterpart_paths(path: &str) -> Vec<String> {
    let dir = path_dir(path);
    let stem = raw_stem(path);
    let ext = path_ext(path);
    let mut out = Vec::new();
    if ext == "go" && stem.ends_with("_test") {
        push_joined(
            &mut out,
            &dir,
            format!("{}.go", stem.trim_end_matches("_test")),
        );
    } else if ext == "go" {
        push_joined(&mut out, &dir, format!("{stem}_test.go"));
    }

    if matches!(ext.as_str(), "ts" | "tsx" | "js" | "jsx") {
        for suffix in [".test", ".spec"] {
            if let Some(base) = stem.strip_suffix(suffix) {
                push_joined(&mut out, &dir, format!("{base}.{ext}"));
            } else {
                push_joined(&mut out, &dir, format!("{stem}{suffix}.{ext}"));
            }
        }
    }
    out
}

fn manifest_counterpart_paths(path: &str) -> Vec<String> {
    match path {
        "package.json" => vec![
            "pnpm-lock.yaml".to_owned(),
            "package-lock.json".to_owned(),
            "yarn.lock".to_owned(),
        ],
        "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" => vec!["package.json".to_owned()],
        "Cargo.toml" => vec!["Cargo.lock".to_owned()],
        "Cargo.lock" => vec!["Cargo.toml".to_owned()],
        "go.mod" => vec!["go.sum".to_owned()],
        "go.sum" => vec!["go.mod".to_owned()],
        _ => Vec::new(),
    }
}

fn common_changed_ancestor_dirs(changed_files: &[String]) -> Vec<String> {
    let dirs = changed_files
        .iter()
        .map(|file| path_dir(&normalize_path(file)))
        .filter(|dir| !dir.is_empty())
        .collect::<Vec<_>>();
    if dirs.is_empty() {
        return Vec::new();
    }
    let common = dirs
        .iter()
        .skip(1)
        .fold(dirs[0].clone(), |acc, dir| common_dir_prefix(&acc, dir));
    let mut out = Vec::new();
    let mut current = common;
    while !current.is_empty() && out.len() < 3 {
        out.push(current.clone());
        current = path_dir(&current);
    }
    out
}

fn changed_extensions(changed_files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for file in changed_files {
        let ext = path_ext(file);
        if !ext.is_empty() && !out.iter().any(|existing| existing == &ext) {
            out.push(ext);
        }
    }
    out
}

fn push_existing_hint(out: &mut Vec<String>, root: &Path, candidate: &str) {
    let candidate = normalize_path(candidate);
    if candidate.is_empty() || out.iter().any(|existing| existing == &candidate) {
        return;
    }
    if root_relative_path(root, &candidate).exists() {
        out.push(candidate);
    }
}

fn root_relative_path(root: &Path, relative: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in normalize_path(relative).split('/') {
        if !part.is_empty() {
            out.push(part);
        }
    }
    out
}

fn push_joined(out: &mut Vec<String>, dir: &str, file: String) {
    if dir.is_empty() {
        out.push(file);
    } else {
        out.push(format!("{dir}/{file}"));
    }
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .to_owned()
}

fn path_dir(path: &str) -> String {
    normalize_path(path)
        .rsplit_once('/')
        .map_or_else(String::new, |(dir, _)| dir.to_owned())
}

fn path_name(path: &str) -> String {
    normalize_path(path)
        .rsplit_once('/')
        .map_or_else(|| normalize_path(path), |(_, name)| name.to_owned())
}

fn raw_stem(path: &str) -> String {
    let name = path_name(path);
    name.rsplit_once('.')
        .map_or(name.clone(), |(stem, _)| stem.to_owned())
}

fn canonical_stem(path: &str) -> String {
    let mut stem = raw_stem(path).to_ascii_lowercase();
    for suffix in ["_test", ".test", ".spec", "-test", "-spec"] {
        if let Some(base) = stem.strip_suffix(suffix) {
            stem = base.to_owned();
            break;
        }
    }
    stem
}

fn path_ext(path: &str) -> String {
    path_name(path)
        .rsplit_once('.')
        .map_or_else(String::new, |(_, ext)| ext.to_ascii_lowercase())
}

fn is_test_path(path: &str) -> bool {
    let lower = normalize_path(path).to_ascii_lowercase();
    lower.contains("/__tests__/")
        || lower.contains("/tests/")
        || lower.ends_with("_test.go")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.tsx")
        || lower.ends_with(".test.js")
        || lower.ends_with(".test.jsx")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.tsx")
        || lower.ends_with(".spec.js")
        || lower.ends_with(".spec.jsx")
}

fn is_ancestor_dir(ancestor: &str, child: &str) -> bool {
    !ancestor.is_empty() && child.starts_with(&format!("{ancestor}/"))
}

fn common_prefix_segments(left: &str, right: &str) -> usize {
    left.split('/')
        .zip(right.split('/'))
        .take_while(|(a, b)| !a.is_empty() && a == b)
        .count()
}

fn common_dir_prefix(left: &str, right: &str) -> String {
    left.split('/')
        .zip(right.split('/'))
        .take_while(|(a, b)| a == b)
        .map(|(part, _)| part)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_local_source_pair_above_distant_history() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("context.go"), "").expect("write source");
        let prediction = serde_json::json!({
            "coedit_file_hints": [
                { "file": "binding/binding_test.go" },
                { "file": "context.go" },
                { "file": "logger_test.go" }
            ],
            "neighbors": []
        });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &["context_test.go".to_owned()],
            root.path(),
        );

        assert_eq!(hints[0], "context.go");
    }

    #[test]
    fn adds_common_directory_index_as_local_hint() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(
            root.path()
                .join("packages/vite/src/node/server/environments"),
        )
        .expect("mkdir env");
        std::fs::create_dir_all(
            root.path()
                .join("packages/vite/src/node/server/middlewares"),
        )
        .expect("mkdir middleware");
        std::fs::write(
            root.path().join("packages/vite/src/node/server/index.ts"),
            "",
        )
        .expect("write index");
        let prediction = serde_json::json!({ "coedit_file_hints": [], "neighbors": [] });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &[
                "packages/vite/src/node/server/environments/fullBundleEnvironment.ts".to_owned(),
                "packages/vite/src/node/server/middlewares/rejectNoCorsRequest.ts".to_owned(),
            ],
            root.path(),
        );

        assert_eq!(hints[0], "packages/vite/src/node/server/index.ts");
    }

    #[test]
    fn adds_relative_import_targets_from_test_changes() {
        // Tests that touch a spec file should hint at the source under
        // test (the imported relative path). For non-test source files
        // we deliberately do NOT follow imports — holdout validation
        // showed it's the dominant false-positive source.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("packages/form-core/src")).expect("mkdir src");
        std::fs::create_dir_all(root.path().join("packages/form-core/tests")).expect("mkdir tests");
        std::fs::write(
            root.path()
                .join("packages/form-core/tests/FieldApi.spec.ts"),
            "import { FieldApi } from '../src/FieldApi'\n\
             import { defaultValidationLogic } from '../src/ValidationLogic'\n",
        )
        .expect("write spec");
        std::fs::write(root.path().join("packages/form-core/src/FieldApi.ts"), "")
            .expect("write field");
        std::fs::write(
            root.path()
                .join("packages/form-core/src/ValidationLogic.ts"),
            "",
        )
        .expect("write validation");
        let prediction = serde_json::json!({
            "coedit_file_hints": [
                { "file": "packages/form-core/src/FieldApi.ts", "score": 0.5, "in_n_of_neighbors": 3 }
            ],
            "neighbors": []
        });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &["packages/form-core/tests/FieldApi.spec.ts".to_owned()],
            root.path(),
        );

        assert!(
            hints
                .iter()
                .any(|h| h == "packages/form-core/src/ValidationLogic.ts"),
            "expected ValidationLogic.ts as relative-import hint from test change, got {hints:?}"
        );
    }

    #[test]
    fn drops_low_confidence_corpus_hints() {
        // A coedit hint with `in_n_of_neighbors == 1` AND `score < 0.2`
        // is essentially noise (one PR weakly co-touched the path). The
        // ranked_coedit_files filter should drop it. The high-confidence
        // hint should still survive.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg/cmd")).expect("mkdir");
        std::fs::write(root.path().join("pkg/cmd/keep.go"), "").expect("keep");
        std::fs::write(root.path().join("pkg/cmd/drop.go"), "").expect("drop");
        let prediction = serde_json::json!({
            "coedit_file_hints": [
                { "file": "pkg/cmd/drop.go", "in_n_of_neighbors": 1, "score": 0.13 },
                { "file": "pkg/cmd/keep.go", "in_n_of_neighbors": 2, "score": 0.18 },
            ],
            "neighbors": []
        });
        let hints = missing_file_hints_from_prediction(
            &prediction,
            &["pkg/cmd/something.go".to_owned()],
            root.path(),
        );
        assert!(hints.iter().any(|h| h == "pkg/cmd/keep.go"));
        assert!(
            !hints.iter().any(|h| h == "pkg/cmd/drop.go"),
            "low-confidence hint should have been filtered, got {hints:?}"
        );
    }

    #[test]
    fn adds_routes_referenced_by_e2e_url_literals() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("e2e/solid-start/basic/tests"))
            .expect("mkdir tests");
        std::fs::create_dir_all(root.path().join("e2e/solid-start/basic/src/routes"))
            .expect("mkdir routes");
        std::fs::write(
            root.path()
                .join("e2e/solid-start/basic/tests/streaming.spec.ts"),
            "await page.goto('/deferred')\nawait page.goto('/deferred-without-suspense')\n",
        )
        .expect("write spec");
        std::fs::write(
            root.path()
                .join("e2e/solid-start/basic/src/routes/deferred-without-suspense.tsx"),
            "",
        )
        .expect("write route");
        let prediction = serde_json::json!({
            "coedit_file_hints": [
                { "file": "e2e/solid-start/basic/src/routes/__root.tsx" }
            ],
            "neighbors": []
        });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &["e2e/solid-start/basic/tests/streaming.spec.ts".to_owned()],
            root.path(),
        );

        assert!(
            hints.iter().any(
                |hint| hint == "e2e/solid-start/basic/src/routes/deferred-without-suspense.tsx"
            )
        );
    }

    #[test]
    fn adds_workflow_siblings_for_workflow_prs() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join(".github/workflows")).expect("mkdir workflows");
        for name in ["autofix.yml", "pr.yml", "release.yml"] {
            std::fs::write(root.path().join(".github/workflows").join(name), "")
                .expect("write workflow");
        }
        let prediction = serde_json::json!({ "coedit_file_hints": [], "neighbors": [] });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &[
                ".github/workflows/pr.yml".to_owned(),
                ".github/workflows/release.yml".to_owned(),
            ],
            root.path(),
        );

        assert_eq!(hints[0], ".github/workflows/autofix.yml");
    }

    #[test]
    fn adds_cli_acceptance_fixtures_for_command_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg/cmd/run/view")).expect("mkdir command");
        std::fs::create_dir_all(root.path().join("acceptance/testdata/workflow"))
            .expect("mkdir fixtures");
        for name in ["run-view.txtar", "run-view-log-escape-sequences.txtar"] {
            std::fs::write(
                root.path().join("acceptance/testdata/workflow").join(name),
                "",
            )
            .expect("write fixture");
        }
        let prediction = serde_json::json!({
            "coedit_file_hints": [
                { "file": "pkg/cmd/copilot/copilot.go" }
            ],
            "neighbors": []
        });

        let hints = missing_file_hints_from_prediction(
            &prediction,
            &["pkg/cmd/run/view/view_test.go".to_owned()],
            root.path(),
        );

        assert!(
            hints
                .iter()
                .any(|hint| hint
                    == "acceptance/testdata/workflow/run-view-log-escape-sequences.txtar")
        );
    }
}
