use std::path::PathBuf;

use serde_json::json;

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{confirm_destructive, exit_err, json_compact_or, project_path};

fn target_path(path: Option<PathBuf>) -> PathBuf {
    path.unwrap_or_else(|| PathBuf::from(project_path()))
}

pub(crate) async fn handle_alias_set(
    ctx: &CommandContext,
    repo: String,
    path: Option<PathBuf>,
    json: bool,
) {
    let path = target_path(path);
    let alias = difflore_core::repo_aliases::set_manual_alias(&ctx.db, &path, &repo)
        .await
        .unwrap_or_else(|err| exit_err(&format!("failed to set repo alias: {err}")));

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "set",
                    "alias": alias,
                }),
                "{}"
            )
        );
        return;
    }

    println!("{}", style::title("Repo Alias Saved"));
    println!("  path: {}", alias.root_path);
    println!("  repo: {}", style::ident(&alias.repo_scope));
    println!();
    println!("  check: {}", style::cmd("difflore status"));
}

pub(crate) async fn handle_alias_list(ctx: &CommandContext, json: bool) {
    let aliases = difflore_core::repo_aliases::list_aliases(&ctx.db)
        .await
        .unwrap_or_else(|err| exit_err(&format!("failed to list repo aliases: {err}")));

    if json {
        println!("{}", json_compact_or(&json!({ "aliases": aliases }), "{}"));
        return;
    }

    println!("{}", style::title("Repo Aliases"));
    if aliases.is_empty() {
        println!("  no manual repo aliases");
        println!(
            "  add: {}",
            style::cmd("difflore repo alias set owner/repo")
        );
        return;
    }

    for alias in aliases {
        println!("  {} {}", style::ident(&alias.repo_scope), alias.root_path);
    }
}

pub(crate) async fn handle_alias_clear(
    ctx: &CommandContext,
    path: Option<PathBuf>,
    yes: bool,
    json: bool,
) {
    let path = target_path(path);
    let matching = difflore_core::repo_aliases::aliases_for_path(&ctx.db, &path)
        .await
        .unwrap_or_else(|err| exit_err(&format!("failed to load repo aliases: {err}")));

    if !matching.is_empty()
        && let Err(err) = confirm_destructive(
            yes,
            &format!(
                "clear {} repo alias(es) for {}?",
                matching.len(),
                path.display()
            ),
        )
    {
        exit_err(&err.to_string());
    }

    let cleared = difflore_core::repo_aliases::clear_manual_aliases_for_path(&ctx.db, &path)
        .await
        .unwrap_or_else(|err| exit_err(&format!("failed to clear repo aliases: {err}")));

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "clear",
                    "path": path.display().to_string(),
                    "cleared": cleared,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "{} Cleared {} repo alias(es).",
        style::ok(style::sym::OK),
        cleared
    );
}
