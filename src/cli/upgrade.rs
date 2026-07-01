//! `shion upgrade` — pull the latest source, rebuild + reinstall the binary, and
//! restart the macOS launchd gateway so the new build goes live.
//!
//! shion's analog of hermes' `hermes update` (git pull → reinstall → restart),
//! minus hermes' fork-sync / Windows-ZIP / hangup machinery — shion is a
//! single-user macOS/Rust tool, so the flow stays small:
//!
//!   1. `git pull --ff-only` in the repo this binary was built from
//!      (`CARGO_MANIFEST_DIR`, baked in at compile time).
//!   2. `cargo install --path <repo> --force`, reinstalled to the **currently
//!      running** binary's location.
//!   3. `shion gateway restart` on macOS — but only if the gateway is actually
//!      loaded under launchd, so an upgrade never installs the service uninvited.
//!      Docker/Linux deployments should restart the container/process outside
//!      shion after upgrading.

use std::path::PathBuf;
use std::process::Command;

use super::service;

pub fn run(no_restart: bool) -> anyhow::Result<()> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if !repo.exists() {
        anyhow::bail!(
            "source repo {} no longer exists — `shion upgrade` rebuilds from the \
             checkout this binary was built in. Re-clone it (or rebuild from the new \
             location) and `cargo install --path .` to relink.",
            repo.display()
        );
    }
    println!(
        "shion upgrade — current version {}\nrepo: {}",
        env!("CARGO_PKG_VERSION"),
        repo.display()
    );

    // 1. Pull latest source. `--ff-only` never silently rewrites local work: a
    //    dirty tree or diverged history surfaces as a git error rather than a
    //    surprise reset.
    if repo.join(".git").exists() {
        println!("\n→ git pull --ff-only");
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["pull", "--ff-only"])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run git (is it installed?): {e}"))?
            .success();
        if !ok {
            anyhow::bail!(
                "git pull failed (uncommitted changes, diverged history, or no network?) — \
                 resolve it in {} and retry",
                repo.display()
            );
        }
    } else {
        println!("\n(not a git repo — skipping pull, rebuilding from the current checkout)");
    }

    // 2. Rebuild + reinstall. Overwriting the running binary's file is safe
    //    (the live process keeps its inode; the path gets the new bytes), and
    //    pointing `--root` at the running binary's location means the next
    //    gateway restart actually launches the new build.
    println!("\n→ cargo install --path . --force (rebuild + reinstall)");
    let mut cmd = Command::new("cargo");
    cmd.arg("install").arg("--path").arg(&repo).arg("--force");
    if let Some(root) = cargo_root_for_current_exe() {
        println!("  installing into {}/bin", root.display());
        cmd.arg("--root").arg(&root);
    }
    let ok = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run cargo: {e}"))?
        .success();
    if !ok {
        anyhow::bail!(
            "cargo install failed — the gateway was NOT restarted and the old binary is \
             still live, so you are not left in a broken state"
        );
    }

    // 3. Restart the gateway so the new binary goes live — only if it is being
    //    supervised by shion itself. Docker/Linux deployments run the gateway in
    //    the foreground and should be restarted by the outer supervisor.
    if no_restart {
        println!("\n--no-restart: skipped. Run `shion gateway restart` to go live.");
    } else if service::gateway_loaded().unwrap_or(false) {
        println!("\n→ restarting gateway");
        service::restart()?;
    } else {
        println!("\ngateway not running under a supervisor — nothing to restart.");
    }

    println!("\n✓ upgrade complete.");
    Ok(())
}

/// The `cargo install --root` that lands the new binary back on the currently
/// running binary's path: if the live binary is `<root>/bin/shion`, return
/// `<root>` (so `cargo install --root <root>` writes `<root>/bin/shion`).
/// Returns `None` — meaning the default cargo bin dir — when the layout isn't
/// the standard `.../bin/<exe>` (e.g. a `cargo run` dev build under `target/`).
fn cargo_root_for_current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    if bin_dir.file_name()?.to_str()? != "bin" {
        return None;
    }
    Some(bin_dir.parent()?.to_path_buf())
}
