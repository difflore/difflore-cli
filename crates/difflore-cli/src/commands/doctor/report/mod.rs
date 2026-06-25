// `writeln!` on a `String` is infallible but returns `fmt::Result`; this
// macro swallows the unused `Ok(())` without scattering `let _` everywhere.
macro_rules! sw {
    ($s:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        let _ = writeln!($s, $($arg)*);
    }};
}

mod env_probes;
mod formatters;
mod hook_chain;
mod validators;

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use env_probes::{
    cloud_section, database_section, env_and_git_section, hook_activity_section,
    injection_paths_section, memory_pipeline_section, paths_section, platform_section,
    rules_origin_section, startup_section, sync_timestamps_section, versions_section,
};
use formatters::{
    daemon_section, distribution_section, embedding_section, footer_section, mcp_section,
    settings_section,
};

pub(crate) async fn build_doctor_report(ctx: &crate::runtime::CommandContext) -> String {
    let mut s = String::new();
    sw!(s, "# difflore doctor report");
    sw!(
        s,
        "\n_Generated {}_\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );

    versions_section(&mut s).await;
    platform_section(&mut s);
    env_and_git_section(&mut s);

    let (cloud_logged_in, cloud_probe) = startup_section(ctx, &mut s).await;
    paths_section(&mut s);
    database_section(ctx, &mut s).await;

    let hook_summary = hook_activity_section(&mut s);
    hook_chain::hook_chain_section(&mut s);
    injection_paths_section(&mut s);
    rules_origin_section(ctx, &mut s).await;
    memory_pipeline_section(&mut s);
    sync_timestamps_section(ctx, &mut s, &cloud_probe).await;
    cloud_section(&mut s, cloud_logged_in, &cloud_probe, &hook_summary).await;
    embedding_section(ctx, &mut s).await;
    daemon_section(ctx, &mut s).await;
    distribution_section(&mut s);
    mcp_section(ctx, &mut s).await;
    settings_section(&mut s);
    debug_log_tail_section(&mut s);
    footer_section(&mut s);

    s
}

const DEBUG_LOG_TAIL_BYTES: u64 = 64 * 1024;
const DEBUG_LOG_TAIL_LINES: usize = 160;
const DEBUG_LOG_FILES: &[&str] = &[
    "hook-daemon.log",
    "hook-daemon.log.1",
    "memory-autopilot.log",
    "memory-autopilot.log.1",
    "outbox-daemon.log",
    "outbox-daemon.log.1",
];

fn debug_log_tail_section(s: &mut String) {
    sw!(s, "\n## · Debug log tail (redacted)\n");
    sw!(
        s,
        "- scope: daemon stderr only; activity events are summarized above, not embedded raw"
    );
    sw!(
        s,
        "- cap: last {DEBUG_LOG_TAIL_LINES} lines / {} KiB per file",
        DEBUG_LOG_TAIL_BYTES / 1024
    );

    let Ok(home) = difflore_core::infra::paths::data_home() else {
        sw!(s, "- logs: data home unavailable");
        return;
    };
    let logs_dir = home.join("logs");
    let mut included = 0usize;
    for name in DEBUG_LOG_FILES {
        let path = logs_dir.join(name);
        if !path.exists() {
            continue;
        }
        included += 1;
        append_debug_log_tail(s, name, &path);
    }
    if included == 0 {
        sw!(s, "- logs: none found under `{}`", logs_dir.display());
    }
}

fn append_debug_log_tail(s: &mut String, name: &str, path: &Path) {
    match read_log_tail(path, DEBUG_LOG_TAIL_BYTES, DEBUG_LOG_TAIL_LINES) {
        Ok(LogTail {
            content,
            bytes_truncated,
            lines_truncated,
        }) if content.trim().is_empty() => {
            sw!(s, "- `{name}`: empty");
            if bytes_truncated || lines_truncated {
                sw!(s, "  - note: earlier content omitted by report cap");
            }
        }
        Ok(LogTail {
            content,
            bytes_truncated,
            lines_truncated,
        }) => {
            let mut notes = Vec::new();
            if bytes_truncated {
                notes.push("older bytes omitted");
            }
            if lines_truncated {
                notes.push("older lines omitted");
            }
            let suffix = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };
            sw!(s, "\n### `{name}`{suffix}\n");
            sw!(s, "```text");
            sw!(s, "{}", content.trim_end());
            sw!(s, "```");
        }
        Err(e) => {
            sw!(s, "- `{}`: failed to read ({e})", path.display());
        }
    }
}

struct LogTail {
    content: String,
    bytes_truncated: bool,
    lines_truncated: bool,
}

fn read_log_tail(path: &Path, max_bytes: u64, max_lines: usize) -> std::io::Result<LogTail> {
    let metadata = std::fs::metadata(path)?;
    let bytes_truncated = metadata.len() > max_bytes;
    let start = metadata.len().saturating_sub(max_bytes);
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut raw = vec![0; metadata.len().saturating_sub(start) as usize];
    file.read_exact(&mut raw)?;
    let raw = String::from_utf8_lossy(&raw);
    Ok(sanitize_log_tail(&raw, max_lines, bytes_truncated))
}

fn sanitize_log_tail(raw: &str, max_lines: usize, bytes_truncated: bool) -> LogTail {
    let lines = raw.lines().collect::<Vec<_>>();
    let drop = lines.len().saturating_sub(max_lines);
    let kept = lines[drop..].join("\n");
    LogTail {
        content: difflore_core::observability::privacy::redact_secrets(&kept),
        bytes_truncated,
        lines_truncated: drop > 0,
    }
}

#[cfg(test)]
fn debug_log_paths_for_report(home: &Path) -> Vec<std::path::PathBuf> {
    DEBUG_LOG_FILES
        .iter()
        .map(|name| home.join("logs").join(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{debug_log_paths_for_report, sanitize_log_tail};

    #[test]
    fn log_tail_redacts_secret_shapes_and_keeps_recent_lines() {
        let raw = [
            "old line",
            "Authorization: Bearer A1b2C3d4E5f6G7h8",
            "new line",
        ]
        .join("\n");

        let tail = sanitize_log_tail(&raw, 2, false);

        assert!(!tail.content.contains("A1b2C3d4E5f6G7h8"));
        assert!(tail.content.contains("new line"));
        assert!(!tail.content.contains("old line"));
        assert!(tail.lines_truncated);
    }

    #[test]
    fn report_debug_log_paths_include_rotated_daemon_logs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = debug_log_paths_for_report(dir.path());
        let names = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert!(names.contains(&"hook-daemon.log.1".to_owned()));
        assert!(names.contains(&"memory-autopilot.log.1".to_owned()));
        assert!(names.contains(&"outbox-daemon.log.1".to_owned()));
    }
}
