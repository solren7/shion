//! The pairing gate channels consult before handing a message to the agent.
//!
//! Ordering of checks: config `allow_from` (pre-trusted, no db hit) → an
//! approved pairing row → otherwise mint/reuse a pending code and tell the
//! sender how to pair. Approval happens out-of-band via `shion pair approve`,
//! which writes the shared SQLite db — the gateway picks it up on the
//! sender's next message, no restart needed.

use std::sync::Arc;

use crate::domain::pairing::{PairingRepository, PairingRequest, PairingStatus};

/// Outcome of the gate for one inbound message.
pub enum Gate {
    Allowed,
    /// Not paired: do not run the agent; send `reply` (the pairing prompt)
    /// back on the channel instead.
    Denied {
        reply: String,
    },
}

pub struct PairingGuard {
    platform: &'static str,
    allow_from: Vec<String>,
    pairings: Arc<dyn PairingRepository>,
}

impl PairingGuard {
    pub fn new(
        platform: &'static str,
        allow_from: Vec<String>,
        pairings: Arc<dyn PairingRepository>,
    ) -> Self {
        Self {
            platform,
            allow_from,
            pairings,
        }
    }

    pub async fn check(&self, sender_id: &str, chat_id: &str) -> anyhow::Result<Gate> {
        if self.allow_from.iter().any(|s| s == sender_id) {
            return Ok(Gate::Allowed);
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        match self.pairings.find(self.platform, sender_id).await? {
            Some(p) if p.status == PairingStatus::Approved => Ok(Gate::Allowed),
            Some(p) if !p.is_expired(now) => Ok(Gate::Denied {
                reply: prompt(&p.code),
            }),
            // First contact, or the previous code expired: mint a fresh one.
            _ => {
                let request = PairingRequest::new(self.platform, sender_id, chat_id);
                self.pairings.upsert(&request).await?;
                Ok(Gate::Denied {
                    reply: prompt(&request.code),
                })
            }
        }
    }
}

fn prompt(code: &str) -> String {
    format!(
        "此账号尚未与 shion 配对。配对码: {code}\n\
         请在 shion 所在主机上运行: shion pair approve {code}\n\
         配对完成后再发消息即可对话。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pairing::PAIRING_CODE_TTL_SECS;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemPairings {
        rows: Mutex<Vec<PairingRequest>>,
    }

    #[async_trait]
    impl PairingRepository for MemPairings {
        async fn upsert(&self, request: &PairingRequest) -> anyhow::Result<()> {
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|r| r.id != request.id);
            rows.push(request.clone());
            Ok(())
        }
        async fn find(
            &self,
            platform: &str,
            sender_id: &str,
        ) -> anyhow::Result<Option<PairingRequest>> {
            let id = format!("{platform}:{sender_id}");
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned())
        }
        async fn approve_code(&self, code: &str) -> anyhow::Result<Option<PairingRequest>> {
            let mut rows = self.rows.lock().unwrap();
            for r in rows.iter_mut() {
                if r.code == code && r.status == PairingStatus::Pending {
                    r.status = PairingStatus::Approved;
                    return Ok(Some(r.clone()));
                }
            }
            Ok(None)
        }
        async fn list(&self) -> anyhow::Result<Vec<PairingRequest>> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn revoke(&self, id: &str) -> anyhow::Result<bool> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|r| r.id != id);
            Ok(rows.len() != before)
        }
    }

    fn guard(allow_from: Vec<String>) -> (PairingGuard, Arc<MemPairings>) {
        let repo = Arc::new(MemPairings::default());
        (
            PairingGuard::new("telegram", allow_from, repo.clone()),
            repo,
        )
    }

    #[tokio::test]
    async fn allow_from_sender_skips_pairing() {
        let (guard, repo) = guard(vec!["42".to_string()]);
        assert!(matches!(
            guard.check("42", "42").await.unwrap(),
            Gate::Allowed
        ));
        assert!(repo.rows.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_sender_gets_pairing_code_and_is_denied() {
        let (guard, repo) = guard(vec![]);
        let Gate::Denied { reply } = guard.check("7", "7").await.unwrap() else {
            panic!("unknown sender must be denied");
        };
        let code = repo.rows.lock().unwrap()[0].code.clone();
        assert!(reply.contains(&code), "{reply}");
        assert!(reply.contains("shion pair approve"), "{reply}");
    }

    #[tokio::test]
    async fn repeated_messages_reuse_the_same_pending_code() {
        let (guard, repo) = guard(vec![]);
        guard.check("7", "7").await.unwrap();
        let first = repo.rows.lock().unwrap()[0].code.clone();
        let Gate::Denied { reply } = guard.check("7", "7").await.unwrap() else {
            panic!("still unpaired");
        };
        assert!(reply.contains(&first), "{reply}");
        assert_eq!(repo.rows.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn approved_sender_is_allowed() {
        let (guard, repo) = guard(vec![]);
        guard.check("7", "7").await.unwrap();
        let code = repo.rows.lock().unwrap()[0].code.clone();
        repo.approve_code(&code).await.unwrap().expect("approves");
        assert!(matches!(
            guard.check("7", "7").await.unwrap(),
            Gate::Allowed
        ));
    }

    #[tokio::test]
    async fn expired_pending_code_is_replaced() {
        let (guard, repo) = guard(vec![]);
        guard.check("7", "7").await.unwrap();
        let first = {
            let mut rows = repo.rows.lock().unwrap();
            rows[0].created_at -= PAIRING_CODE_TTL_SECS + 60;
            rows[0].code.clone()
        };
        guard.check("7", "7").await.unwrap();
        let rows = repo.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].code, first, "expired code must be reissued");
    }
}
