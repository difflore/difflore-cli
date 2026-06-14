use crate::commands::doctor::{DoctorArgs, handle_doctor};
use crate::installer;
use crate::runtime::CommandContext;
use crate::style::{self, sym};

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
    print_binary_update_hint();
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
            report: false,
            fix: false,
            drain_abandoned: false,
            older_than: "30d".to_owned(),
            no_dry_run: false,
            json: false,
        },
    )
    .await;
}

fn print_binary_update_hint() {
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "  {} binary: difflore {current}",
        style::pewter(sym::BULLET)
    );
    match self_update_hint() {
        Some(command) => println!(
            "  {} update binary separately when needed: {}",
            style::pewter(sym::BULLET),
            style::cmd(command),
        ),
        None => println!(
            "  {} binary channel unknown; use your original install method, then run {}",
            style::pewter(sym::BULLET),
            style::cmd("difflore update"),
        ),
    }
}

fn self_update_hint() -> Option<&'static str> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_string_lossy();
    if exe.contains("/.cargo/bin/") {
        Some("cargo install difflore-cli --locked")
    } else if exe.contains("/homebrew/")
        || exe.contains("/Cellar/")
        || exe.starts_with("/opt/homebrew/")
        || exe.starts_with("/usr/local/")
    {
        Some("brew upgrade difflore")
    } else if exe.contains("/.difflore/bin/") {
        Some(
            "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/difflore/difflore-cli/releases/latest/download/difflore-cli-installer.sh | sh",
        )
    } else {
        None
    }
}
