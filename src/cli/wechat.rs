//! `komo wechat login` — interactive QR provisioning for the WeChat channel.
//!
//! The gateway can't render a QR code (it runs headless under launchd), so the
//! one interactive step — scanning a login QR with the WeChat app — is an
//! operator command run on the host. It writes the iLink credentials to
//! `~/.komo/wechat/credentials.json`; the gateway channel then reuses them
//! non-interactively. Re-provision by deleting that file and running this again.

use qrcode::{QrCode, render::unicode};
use wechatbot::{BotOptions, WeChatBot};

fn render_qr(content: &str) {
    match QrCode::new(content.as_bytes()) {
        Ok(code) => {
            let img = code
                .render::<unicode::Dense1x2>()
                .dark_color(unicode::Dense1x2::Light)
                .light_color(unicode::Dense1x2::Dark)
                .build();
            eprintln!("\n{img}\n用微信扫上面的二维码，然后在手机上确认。\n");
        }
        // Not encodable as a QR? Fall back to the raw string.
        Err(_) => eprintln!("\n[二维码内容] {content}\n"),
    }
}

pub async fn login() -> anyhow::Result<()> {
    let cred_path = crate::config::wechat_cred_path();
    let bot = WeChatBot::new(BotOptions {
        cred_path: Some(cred_path.to_string_lossy().into_owned()),
        on_qr_url: Some(Box::new(render_qr)),
        on_error: Some(Box::new(|e| eprintln!("[wechat] {e}"))),
        ..Default::default()
    });

    println!("登录微信（已有凭证则直接复用，否则请扫码）…");
    let creds = bot
        .login(false)
        .await
        .map_err(|e| anyhow::anyhow!("wechat login failed: {e}"))?;
    println!(
        "✅ 已登录\n  user_id    = {}\n  account_id = {}",
        creds.user_id, creds.account_id
    );
    println!("凭证已保存到 {}", cred_path.display());
    println!("现在可以在 ~/.komo/config.toml 启用 [channels.wechat] 并重启 gateway。");
    Ok(())
}
