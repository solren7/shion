//! OS-level supervisor for the gateway.
//!
//! macOS uses `launchd`: `shion gateway start` writes a LaunchAgent plist and
//! bootstraps it; launchd then owns the process (`KeepAlive` relaunches it
//! after a crash, `RunAtLoad` starts it at login).
//!
//! Other platforms, including Linux containers, should run `shion gateway` in
//! the foreground and let the outer supervisor (Docker, Compose, systemd, etc.)
//! own start/stop/restart.

/// Write the plist and bootstrap the gateway under launchd.
pub fn start() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::start()
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsupported("start")
    }
}

/// Stop the launchd-managed gateway.
pub fn stop() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::stop()
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsupported("stop")
    }
}

/// Stop (if running) and start again — picks up a rebuilt/reinstalled binary.
pub fn restart() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::restart()
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsupported("restart")
    }
}

/// Report whether launchd has the gateway loaded.
pub fn status() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsupported("status")
    }
}

/// Whether a supervised gateway is currently live. `shion upgrade` uses this to
/// decide whether to restart — so an upgrade never *installs* the supervisor for
/// someone who only runs the gateway in the foreground.
pub fn gateway_loaded() -> anyhow::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let domain = launchd::gui_domain()?;
        Ok(launchd::is_loaded(&domain))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(false)
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported(action: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "gateway {action} is macOS-only. In Docker/Linux, run `shion gateway` in \
         the foreground and use your supervisor, e.g. `docker restart <container>`."
    )
}

// ---------------------------------------------------------------------------
// macOS: launchd LaunchAgent
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod launchd {
    use std::path::PathBuf;
    use std::process::Command;

    const LABEL: &str = "com.shion.gateway";

    /// Render the LaunchAgent plist. Pure so the XML is unit-testable.
    /// `exe` is the absolute shion binary path; `log_dir` holds stdout/stderr logs;
    /// `work_dir` is the process working directory (launchd defaults to `/`, which
    /// would make the workspace-confined tools useless).
    fn render_plist(exe: &str, log_dir: &str, work_dir: &str) -> String {
        let exe = xml_escape(exe);
        let log_dir = xml_escape(log_dir);
        let work_dir = xml_escape(work_dir);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>gateway</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{work_dir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log_dir}/gateway.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/gateway.err.log</string>
</dict>
</plist>
"#
        )
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn plist_path() -> anyhow::Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// `gui/<uid>` launchd domain for the current user.
    pub(super) fn gui_domain() -> anyhow::Result<String> {
        let out = Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if uid.is_empty() {
            anyhow::bail!("could not determine uid via `id -u`");
        }
        Ok(format!("gui/{uid}"))
    }

    fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
        Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))
    }

    pub(super) fn is_loaded(domain: &str) -> bool {
        launchctl(&["print", &format!("{domain}/{LABEL}")])
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// Poll until launchd has fully unloaded the service, returning whether it did
    /// within the timeout. `bootout` returns before launchd reaps the job, so a
    /// follow-up `start` (which guards on `is_loaded`) would otherwise see it still
    /// present and skip bootstrapping — the restart race.
    fn wait_until_unloaded(domain: &str) -> bool {
        // ~5s budget: launchd usually unloads within a few hundred ms.
        for _ in 0..50 {
            if !is_loaded(domain) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        !is_loaded(domain)
    }

    /// Write the plist and bootstrap it into the user's gui domain.
    pub fn start() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        if is_loaded(&domain) {
            println!(
                "shion gateway is already running under launchd. Use `shion gateway restart` to restart it."
            );
            return Ok(());
        }

        let exe = std::env::current_exe()?;
        let shion_home = crate::config::ensure_shion_home();
        let log_dir = shion_home.join("logs");
        std::fs::create_dir_all(&log_dir)?;

        let path = plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            render_plist(
                &exe.display().to_string(),
                &log_dir.display().to_string(),
                &shion_home.display().to_string(),
            ),
        )?;

        let out = launchctl(&["bootstrap", &domain, &path.display().to_string()])?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!(
            "shion gateway started under launchd ({LABEL}).\n\
             It will restart automatically on crash and start at login.\n\
             Logs: {}/gateway.log",
            log_dir.display()
        );
        Ok(())
    }

    /// Remove the service from launchd (stops the process and disables auto-restart).
    pub fn stop() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        if !is_loaded(&domain) {
            println!("shion gateway is not running under launchd.");
            return Ok(());
        }
        let out = launchctl(&["bootout", &format!("{domain}/{LABEL}")])?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootout failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // Wait for the job to actually unload so the "stopped" report is truthful
        // and an immediate `start` afterwards isn't blocked by the stale job.
        wait_until_unloaded(&domain);
        println!("shion gateway stopped.");
        Ok(())
    }

    /// Stop (if loaded), regenerate the plist, and start again. Regenerating means
    /// a rebuilt/reinstalled binary or moved log dir is picked up on restart.
    pub fn restart() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        if is_loaded(&domain) {
            let out = launchctl(&["bootout", &format!("{domain}/{LABEL}")])?;
            if !out.status.success() {
                anyhow::bail!(
                    "launchctl bootout failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            // bootout is async; wait for the unload to land before `start`, whose
            // `is_loaded` guard would otherwise see the stale job and no-op.
            if !wait_until_unloaded(&domain) {
                anyhow::bail!(
                    "gateway did not unload after bootout; wait a moment and run `shion gateway start`"
                );
            }
        }
        start()
    }

    /// Report whether launchd has the service and whether the process is running.
    pub fn status() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        let out = launchctl(&["print", &format!("{domain}/{LABEL}")])?;
        if !out.status.success() {
            println!("shion gateway: not loaded (run `shion gateway start`).");
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // Surface just the interesting lines from launchctl's verbose dump.
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("state =")
                || trimmed.starts_with("pid =")
                || trimmed.starts_with("path =")
                || trimmed.starts_with("last exit code =")
            {
                println!("{trimmed}");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_contains_label_exe_keepalive_and_workdir() {
            let plist = render_plist(
                "/usr/local/bin/shion",
                "/Users/me/.shion/logs",
                "/Users/me/.shion",
            );
            assert!(plist.contains("<string>com.shion.gateway</string>"));
            assert!(plist.contains("<string>/usr/local/bin/shion</string>"));
            assert!(plist.contains("<string>gateway</string>"));
            assert!(plist.contains("<key>KeepAlive</key>"));
            assert!(plist.contains("/Users/me/.shion/logs/gateway.log"));
            assert!(plist.contains("<key>WorkingDirectory</key>"));
            assert!(plist.contains("<string>/Users/me/.shion</string>"));
        }

        #[test]
        fn plist_escapes_xml_special_chars_in_paths() {
            let plist = render_plist("/odd<&>path/shion", "/logs", "/work");
            assert!(plist.contains("/odd&lt;&amp;&gt;path/shion"));
            assert!(!plist.contains("/odd<&>path"));
        }
    }
}
