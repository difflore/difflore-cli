pub(crate) mod setup;
mod tools;

use crate::style;
use colored::Colorize;
use gate4agent::CliTool;

use tools::{build_model_mapping, default_model_for, provider_name_for};

use difflore_core::domain::models::{
    ProviderAddInput, ProviderRemoveInput, ProviderSetActiveInput,
};

use anyhow::bail;

use crate::runtime::CommandContext;
use crate::support::util::confirm_destructive;

/// Resolve a secret from, in order: explicit flag, env var, or piped stdin.
/// Used by cloud login to pick up `DIFFLORE_CLOUD_TOKEN`.
pub(crate) fn resolve_secret_input(
    flag_value: Option<String>,
    env_var: &str,
    label: &str,
    cmd: &str,
) -> anyhow::Result<String> {
    if let Some(v) = flag_value.filter(|s| !s.trim().is_empty()) {
        return Ok(v);
    }
    if let Some(v) = difflore_core::infra::env::var(env_var)
        && !v.trim().is_empty()
    {
        return Ok(v);
    }
    use std::io::{IsTerminal, Read};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut buf = String::new();
        if stdin.lock().read_to_string(&mut buf).is_ok() {
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
    }
    bail!(
        "{label} required. Supply via one of:\n  \
         1. explicit flag  (discouraged; leaks to shell history)\n  \
         2. {env_var} env var  (recommended)\n  \
         3. echo \"<VALUE>\" | {cmd} ...  (piped stdin)"
    );
}

fn parse_tool(input: &str) -> Result<CliTool, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claude-cli" => Ok(CliTool::ClaudeCode),
        "codex" | "codex-cli" => Ok(CliTool::Codex),
        "gemini" | "gemini-cli" => Ok(CliTool::Gemini),
        "opencode" | "opencode-cli" => Ok(CliTool::OpenCode),
        other => Err(format!(
            "unknown agent CLI '{other}'. Expected one of: claude, codex, gemini, opencode."
        )),
    }
}

pub(crate) async fn handle_providers_list(ctx: &CommandContext, json: bool) -> anyhow::Result<()> {
    let db = &ctx.db;

    let providers = difflore_core::infra::providers::list(db)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list providers: {e}"))?;

    if json {
        let json_out = crate::support::util::json_or(&providers, "[]");
        println!("{json_out}");
        return Ok(());
    }

    if providers.is_empty() {
        println!(
            "{} No providers configured.",
            style::emerald(style::sym::TIP)
        );
        println!(
            "  Run {} to pick one interactively.",
            style::cmd("difflore providers setup")
        );
        return Ok(());
    }

    println!("{} ({} total)\n", style::ok("Providers"), providers.len());
    let mut active_present = false;
    for provider in &providers {
        let active = if provider.is_active {
            active_present = true;
            style::ok(" [active]").to_string()
        } else {
            String::new()
        };
        println!("  {}{}", provider.name.bold(), active);
        println!("    id:        {}", style::pewter(&provider.id));
        println!("    backend:   {}", style::pewter(&provider.base_url));
        if !provider.model_mapping.is_empty() {
            let mappings = provider
                .model_mapping
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("    models:    {}", style::pewter(&mappings));
        }
        println!();
    }

    if active_present {
        println!(
            "  {} next: {}",
            style::emerald(style::sym::TIP),
            style::cmd("difflore review --diff all"),
        );
    } else {
        println!(
            "  {} no active provider — run {}",
            style::amber("!"),
            style::cmd("difflore providers set-active <id>"),
        );
    }

    Ok(())
}

pub(crate) async fn handle_providers_add(
    ctx: &CommandContext,
    tool_input: &str,
    model: Option<&str>,
) -> anyhow::Result<()> {
    let tool = parse_tool(tool_input).map_err(anyhow::Error::msg)?;

    let model_value = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| default_model_for(tool).to_owned(), str::to_owned);

    let input = ProviderAddInput {
        name: provider_name_for(tool).to_owned(),
        base_url: difflore_core::review_engine::agent_cli_sentinel(tool).to_owned(),
        model_mapping: build_model_mapping(&model_value),
    };

    let db = &ctx.db;
    let had_active = difflore_core::infra::providers::list(db)
        .await
        .is_ok_and(|ps| ps.iter().any(|p| p.is_active));
    match difflore_core::infra::providers::add(db, input).await {
        Ok(provider) => {
            if had_active {
                println!(
                    "{} Added provider: {}",
                    style::ok(style::sym::OK),
                    provider.name.bold()
                );
                println!("  ID: {}", style::pewter(&provider.id));
                println!("  Backend: {}", provider.base_url);
                println!();
                println!(
                    "  {} Provider added but not active. To switch to it: {}",
                    style::pewter(style::sym::TIP),
                    style::cmd(&format!("difflore providers set-active {}", provider.name)),
                );
            } else {
                if let Err(e) = difflore_core::infra::providers::set_active(
                    db,
                    ProviderSetActiveInput {
                        id: provider.id.clone(),
                        is_active: true,
                    },
                )
                .await
                {
                    bail!("Added provider but could not activate it: {e}");
                }
                println!(
                    "{} Added and set active: {}",
                    style::ok(style::sym::OK),
                    provider.name.bold()
                );
                println!("  Backend: {}", provider.base_url);
                println!();
                println!(
                    "  {} next: {}",
                    style::emerald(style::sym::TIP),
                    style::cmd("difflore review --diff all"),
                );
            }
        }
        Err(e) => bail!("Failed to add provider: {e}"),
    }

    Ok(())
}

pub(crate) async fn handle_providers_set_active(
    ctx: &CommandContext,
    id: &str,
) -> anyhow::Result<()> {
    let db = &ctx.db;
    let id = resolve_provider_id(ctx, id).await?;
    let input = ProviderSetActiveInput {
        id: id.clone(),
        is_active: true,
    };

    match difflore_core::infra::providers::set_active(db, input).await {
        Ok(()) => {
            println!("{} Active provider set: {}", style::ok(style::sym::OK), id);
            println!();
            println!(
                "  {} next: {}",
                style::emerald(style::sym::TIP),
                style::cmd("difflore review --diff all"),
            );
        }
        Err(e) => bail!("Failed to activate provider: {e}"),
    }

    Ok(())
}

pub(crate) async fn handle_providers_remove(
    ctx: &CommandContext,
    id: &str,
    yes: bool,
) -> anyhow::Result<()> {
    let id = resolve_provider_id(ctx, id).await?;
    confirm_destructive(yes, &format!("remove provider `{id}`?"))?;
    let db = &ctx.db;
    let input = ProviderRemoveInput { id: id.clone() };

    match difflore_core::infra::providers::remove(db, input).await {
        Ok(()) => {
            println!("{} Provider removed: {}", style::ok(style::sym::OK), id);
            let remaining = difflore_core::infra::providers::list(db)
                .await
                .unwrap_or_default();
            let any_active = remaining.iter().any(|p| p.is_active);
            if !any_active && !remaining.is_empty() {
                println!();
                println!(
                    "  {} no active provider remains. Activate another: {}",
                    style::amber(style::sym::WARN),
                    style::cmd("difflore providers list"),
                );
                println!("    (find an id, then `difflore providers set-active <id>`)");
            } else if remaining.is_empty() {
                println!();
                println!(
                    "  {} no providers configured. Re-add interactively: {}",
                    style::pewter(style::sym::TIP),
                    style::cmd("difflore providers setup"),
                );
            }
        }
        Err(e) => bail!("Failed to remove provider: {e}"),
    }

    Ok(())
}

async fn resolve_provider_id(ctx: &CommandContext, input: &str) -> anyhow::Result<String> {
    let needle = input.trim();
    if needle.is_empty() {
        bail!("Provider id or name is required.");
    }
    let providers = difflore_core::infra::providers::list(&ctx.db)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list providers: {e}"))?;

    if let Some(provider) = providers.iter().find(|p| p.id == needle) {
        return Ok(provider.id.clone());
    }

    let needle_lower = needle.to_ascii_lowercase();
    let matches = providers
        .iter()
        .filter(|p| {
            let name = p.name.to_ascii_lowercase();
            let short = name.strip_suffix("-cli").unwrap_or(&name);
            name == needle_lower || short == needle_lower
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [provider] => Ok(provider.id.clone()),
        [] => bail!(
            "Provider not found: {needle}. Run {} to see configured providers.",
            style::cmd("difflore providers list")
        ),
        _ => bail!(
            "Provider name is ambiguous: {needle}. Use an exact id from {}.",
            style::cmd("difflore providers list")
        ),
    }
}
