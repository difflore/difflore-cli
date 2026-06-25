use crate::commands::doctor::{DoctorArgs, handle_doctor};
use crate::installer;
use crate::runtime::CommandContext;
use crate::style::{self, sym};
use std::io::Write as _;
use std::path::Path;
use std::process::Command;

/// URL of the install script piped into the shell on a managed self-update.
const INSTALL_SCRIPT_URL: &str = if cfg!(windows) {
    "https://difflore.dev/install.ps1"
} else {
    "https://difflore.dev/install.sh"
};

/// Pinned SHA-256 of the install script, in `sha256:<hex>` form, verified
/// before the script is executed so a single difflore.dev / TLS-MITM
/// compromise is not instant RCE on every updating client.
///
/// `None` = unpinned. While unpinned we DO NOT silently fall back to the old
/// blind `curl | sh` pipe (that would defeat the purpose); instead the managed
/// self-update refuses to auto-run and tells the user to update manually with
/// the one-line installer they can inspect. Set this at release time once the
/// installer is published & frozen.
///
/// TODO(security, #43): replace this pinned-checksum mechanism with a proper
/// signature scheme — publish a detached signature of install.{ps1,sh} and
/// verify it here against a public key baked into the binary, so the install
/// host does not have to be trusted at all (a checksum still trusts whoever
/// publishes the pinned value at build time).
const PINNED_INSTALL_SCRIPT_SHA256: Option<&str> = None;

#[derive(Debug, Clone, Copy)]
pub(crate) struct UpdateArgs {
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

pub(crate) async fn handle_update(ctx: &CommandContext, args: UpdateArgs) {
    println!(
        "{} {}",
        style::emerald(sym::TIP),
        style::pewter("Checking DiffLore update path"),
    );
    update_binary(args.dry_run).await;
    println!();

    installer::update_all(args.dry_run, args.force);

    if args.dry_run {
        println!();
        println!(
            "  {} dry-run only: skipped doctor. Re-run {} to apply safe agent block updates.",
            style::pewter(sym::BULLET),
            style::cmd("difflore update"),
        );
        return;
    }

    println!();
    println!(
        "{} {}",
        style::emerald(sym::TIP),
        style::pewter("Running doctor after agent block update"),
    );
    handle_doctor(
        ctx,
        DoctorArgs {
            report: None,
            fix: false,
            drain_abandoned: false,
            older_than: "30d".to_owned(),
            no_dry_run: false,
            json: false,
        },
    )
    .await;
}

async fn update_binary(dry_run: bool) {
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "  {} binary: difflore {current}",
        style::pewter(sym::BULLET)
    );
    match self_update_plan() {
        BinaryUpdatePlan::Managed { command } => {
            if dry_run {
                println!(
                    "  {} would refresh installer-managed binary via: {}",
                    style::pewter(sym::BULLET),
                    style::cmd(command),
                );
                return;
            }
            println!(
                "  {} refreshing installer-managed binary via {}",
                style::pewter(sym::BULLET),
                style::cmd(command),
            );
            if let Err(e) = run_installer_update().await {
                eprintln!(
                    "{} binary update failed: {e}. Agent block updates will still run.",
                    style::warn("warning:")
                );
            }
        }
        BinaryUpdatePlan::Manual { reason, command } => {
            println!(
                "  {} {reason}: {}",
                style::pewter(sym::BULLET),
                style::cmd(command),
            );
        }
    }
}

enum BinaryUpdatePlan {
    Managed {
        command: &'static str,
    },
    Manual {
        reason: &'static str,
        command: &'static str,
    },
}

fn self_update_plan() -> BinaryUpdatePlan {
    let exe = std::env::current_exe().ok();
    if exe.as_deref().is_some_and(is_cargo_install) {
        return BinaryUpdatePlan::Manual {
            reason: "Cargo install detected; update manually",
            command: "cargo install difflore-cli --locked",
        };
    }
    if exe.as_deref().is_some_and(is_managed_install) {
        return BinaryUpdatePlan::Managed {
            command: public_install_command(),
        };
    }
    BinaryUpdatePlan::Manual {
        reason: "binary channel unknown; reinstall with the official one-line installer",
        command: public_install_command(),
    }
}

fn is_cargo_install(path: &Path) -> bool {
    let normalized = normalized_path(path);
    normalized.contains("/.cargo/bin/")
}

fn is_managed_install(path: &Path) -> bool {
    let normalized = normalized_path(path);
    normalized.contains("/.difflore/bin/") || normalized.contains("/.difflore/versions/")
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

const fn public_install_command() -> &'static str {
    if cfg!(windows) {
        "irm https://difflore.dev/install.ps1 | iex"
    } else {
        "curl -fsSL https://difflore.dev/install.sh | sh"
    }
}

async fn run_installer_update() -> Result<(), String> {
    // Refuse to auto-run an unverifiable script: without a pinned checksum we
    // can't tell a legitimate installer from one served by a compromised host
    // or TLS MITM, so we never blindly `curl | sh`. The user can still update
    // by running the inspectable one-line installer themselves.
    let Some(expected) = PINNED_INSTALL_SCRIPT_SHA256 else {
        return Err(format!(
            "self-update is not pinned to a verified installer checksum yet; \
             update manually with: {}",
            public_install_command()
        ));
    };

    let script = download_install_script().await?;
    verify_install_script(script.as_bytes(), expected)?;
    run_verified_script(&script)
}

/// Fetch the install script over HTTPS (TLS via rustls). Returns the raw script
/// text; the bytes are checksum-verified by the caller before execution.
async fn download_install_script() -> Result<String, String> {
    let resp = reqwest::Client::new()
        .get(INSTALL_SCRIPT_URL)
        .send()
        .await
        .map_err(|e| format!("could not download installer: {e}"))?
        .error_for_status()
        .map_err(|e| format!("installer download failed: {e}"))?;
    resp.text()
        .await
        .map_err(|e| format!("could not read installer body: {e}"))
}

/// Verify the downloaded script matches the pinned `sha256:<hex>` checksum.
/// Reuses the core SHA-256 helper so the digest format is identical everywhere.
fn verify_install_script(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = difflore_core::infra::crypto::sha256_block_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "installer checksum mismatch (expected {expected}, got {actual}); \
             refusing to execute — possible compromised download"
        ))
    }
}

/// Execute an already-verified install script via the platform shell, passing
/// it as a file rather than re-fetching over the network (so the bytes that ran
/// are exactly the bytes we checksummed).
fn run_verified_script(script: &str) -> Result<(), String> {
    let suffix = if cfg!(windows) { ".ps1" } else { ".sh" };
    let mut file = tempfile::Builder::new()
        .prefix("difflore-install-")
        .suffix(suffix)
        .tempfile()
        .map_err(|e| format!("could not stage installer: {e}"))?;
    file.write_all(script.as_bytes())
        .map_err(|e| format!("could not stage installer: {e}"))?;
    file.flush()
        .map_err(|e| format!("could not stage installer: {e}"))?;
    let path = file.path().to_owned();

    let status = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&path)
            .status()
    } else {
        Command::new("sh").arg(&path).status()
    }
    .map_err(|e| format!("could not start installer: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("installer exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cargo_install_uses_cargo_update_hint() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\.cargo\bin\difflore.exe")
        } else {
            PathBuf::from("/Users/me/.cargo/bin/difflore")
        };
        assert!(is_cargo_install(&path));
        assert!(!is_managed_install(&path));
    }

    #[test]
    fn managed_install_detects_bin_and_version_paths() {
        let bin = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\.difflore\bin\difflore.exe")
        } else {
            PathBuf::from("/Users/me/.difflore/bin/difflore")
        };
        let version = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\.difflore\versions\0.3.0\difflore.exe")
        } else {
            PathBuf::from("/Users/me/.difflore/versions/0.3.0/difflore")
        };
        assert!(is_managed_install(&bin));
        assert!(is_managed_install(&version));
    }

    #[test]
    fn verify_install_script_accepts_matching_checksum() {
        let body = b"#!/bin/sh\necho hi\n";
        let expected = difflore_core::infra::crypto::sha256_block_hex(body);
        assert!(verify_install_script(body, &expected).is_ok());
        // Case-insensitive hex comparison must also pass.
        assert!(verify_install_script(body, &expected.to_uppercase()).is_ok());
    }

    #[test]
    fn verify_install_script_rejects_tampered_body() {
        let expected = difflore_core::infra::crypto::sha256_block_hex(b"original");
        let err = verify_install_script(b"tampered", &expected).unwrap_err();
        assert!(err.contains("checksum mismatch"), "msg: {err}");
    }
}
