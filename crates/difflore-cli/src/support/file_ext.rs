//! Shared file-extension allowlists for classifying source, test, and
//! review-relevant files. Centralized here so the import-reviews scoping
//! and recall file-targeting heuristics stay in lock-step instead of
//! drifting across hand-maintained `matches!` arms.

/// Source-code (and co-located test) file extensions. This is the set that
/// counts as "real code" for repo-wide import broadening and recall's
/// primary-file selection.
pub(crate) const SOURCE_CODE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hpp", "cs", "go", "java", "js", "jsx", "mjs", "cjs", "ts",
    "tsx", "mts", "cts", "py", "rb", "rs", "swift", "kt", "kts", "php", "vue", "svelte",
];

/// Extra non-source extensions that are still worth importing review threads
/// for (configs, docs, scripts). Layered on top of [`SOURCE_CODE_EXTENSIONS`]
/// rather than duplicating the source list.
pub(crate) const REVIEW_EXTRA_EXTENSIONS: &[&str] = &[
    "json", "toml", "yaml", "yml", "md", "sql", "sh", "ps1", "xml", "txtar",
];

/// True when `ext` (lowercased, no leading dot) is a recognized source or
/// test file extension.
pub(crate) fn is_source_code_extension(ext: &str) -> bool {
    SOURCE_CODE_EXTENSIONS.contains(&ext)
}

/// True when `ext` is review-relevant: any source extension, plus the
/// configs/docs/scripts in [`REVIEW_EXTRA_EXTENSIONS`].
pub(crate) fn is_review_file_extension(ext: &str) -> bool {
    is_source_code_extension(ext) || REVIEW_EXTRA_EXTENSIONS.contains(&ext)
}
