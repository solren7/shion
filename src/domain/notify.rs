use async_trait::async_trait;

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}
