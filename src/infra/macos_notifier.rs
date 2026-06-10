use async_trait::async_trait;
use tracing::warn;

use crate::domain::notify::Notifier;

pub struct MacosNotifier;

#[async_trait]
impl Notifier for MacosNotifier {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let script = format!(
            r#"display notification "{}" with title "{}""#,
            escape_applescript(body),
            escape_applescript(title),
        );
        match tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                warn!(
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "osascript notification failed (degrading gracefully)"
                );
            }
            Err(e) => {
                warn!(error = %e, "osascript unavailable (degrading gracefully)");
            }
        }
        Ok(())
    }
}

/// Escape `"` and `\` so user-controlled text cannot break out of the
/// AppleScript string literal. Called for both title and body.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_double_quotes_and_backslashes() {
        assert_eq!(escape_applescript(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
    }
}
