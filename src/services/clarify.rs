//! Mid-turn clarification state: lets the `ask_user` tool pause a turn on a
//! question and resume when the user's next message answers it.
//!
//! Mirrors `agent::interaction::ApprovalState`'s shape — a per-session
//! `oneshot` the tool awaits and the dispatcher resolves — but the payload is
//! the user's free-text answer instead of an approve/deny decision. Lives in
//! `services/` so both sides can reach it without a layering cycle: the
//! `ask_user` tool (in `tools/`) registers and awaits; the gateway dispatcher
//! and the TUI (in `agent/` / `tui/`) resolve an inbound message into it.
//!
//! Design contract (see `.scratch/clarify-step/PRD.md`):
//! - at most one pending question per session (a second `register` supersedes
//!   the first; the superseded waiter reads the dropped sender as "no answer");
//! - a per-turn budget caps how many times a single turn may ask
//!   ([`CLARIFY_BUDGET_PER_TURN`]) so the agent can't interrogate;
//! - `/new` and turn completion clear everything for the session.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::oneshot;

/// How long a clarify question waits for the user before the tool gives up
/// and tells the model to proceed on its best assumption. Longer than the
/// approval timeout (5 min): answering may require the user to look something
/// up, and an unanswered clarify degrades gracefully rather than denying.
pub const CLARIFY_TIMEOUT: Duration = Duration::from_secs(600);

/// How many times one turn may ask the user (anti-interrogation cap).
pub const CLARIFY_BUDGET_PER_TURN: u32 = 2;

/// Shared clarify state, keyed by session id.
pub struct ClarifyState {
    pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
    /// Questions asked in the session's current turn (reset at turn start).
    asked: Mutex<HashMap<String, u32>>,
    pub timeout: Duration,
}

impl ClarifyState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            asked: Mutex::new(HashMap::new()),
            timeout: CLARIFY_TIMEOUT,
        }
    }

    /// Claim one unit of the turn's clarify budget. `false` = exhausted (the
    /// tool reports that to the model instead of asking).
    pub fn try_claim_budget(&self, session: &str) -> bool {
        let mut asked = self.asked.lock().unwrap();
        let count = asked.entry(session.to_string()).or_insert(0);
        if *count >= CLARIFY_BUDGET_PER_TURN {
            return false;
        }
        *count += 1;
        true
    }

    /// Register a pending question for `session`, returning the receiver the
    /// tool awaits. Supersedes any prior pending question (its sender drops,
    /// which the old waiter reads as "no answer").
    pub fn register(&self, session: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(session.to_string(), tx);
        rx
    }

    /// Deliver the user's `answer` to the tool waiting on `session`. Returns
    /// whether a question was actually pending — the caller uses this to
    /// decide between "this message is an answer" and "this message starts a
    /// new turn / queues".
    pub fn resolve(&self, session: &str, answer: &str) -> bool {
        match self.pending.lock().unwrap().remove(session) {
            Some(tx) => tx.send(answer.to_string()).is_ok(),
            None => false,
        }
    }

    /// Whether a question is pending for `session` (used by the TUI to let a
    /// mid-turn submit through as an answer).
    pub fn has_pending(&self, session: &str) -> bool {
        self.pending.lock().unwrap().contains_key(session)
    }

    /// Drop a pending question without answering (waiter reads "no answer").
    pub fn forget_pending(&self, session: &str) {
        self.pending.lock().unwrap().remove(session);
    }

    /// A new turn is starting: reset the budget and drop any stale question.
    pub fn begin_turn(&self, session: &str) {
        self.forget_pending(session);
        self.asked.lock().unwrap().remove(session);
    }

    /// The turn ended (or `/new`): drop pending question and budget state.
    pub fn clear(&self, session: &str) {
        self.forget_pending(session);
        self.asked.lock().unwrap().remove(session);
    }
}

impl Default for ClarifyState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answer_reaches_the_waiter() {
        let state = ClarifyState::new();
        let rx = state.register("s1");
        assert!(state.has_pending("s1"));
        assert!(state.resolve("s1", "the blue one"));
        assert_eq!(rx.await.unwrap(), "the blue one");
        assert!(!state.has_pending("s1"));
    }

    #[tokio::test]
    async fn resolve_without_pending_is_false() {
        let state = ClarifyState::new();
        assert!(!state.resolve("s1", "hello"));
    }

    #[tokio::test]
    async fn clear_drops_the_waiter_as_no_answer() {
        let state = ClarifyState::new();
        let rx = state.register("s1");
        state.clear("s1");
        assert!(rx.await.is_err(), "dropped sender reads as no answer");
    }

    #[test]
    fn budget_caps_per_turn_and_resets_on_begin_turn() {
        let state = ClarifyState::new();
        assert!(state.try_claim_budget("s1"));
        assert!(state.try_claim_budget("s1"));
        assert!(!state.try_claim_budget("s1"), "third ask is over budget");
        // Another session is unaffected.
        assert!(state.try_claim_budget("s2"));
        // A new turn restores the budget.
        state.begin_turn("s1");
        assert!(state.try_claim_budget("s1"));
    }

    #[tokio::test]
    async fn superseding_register_drops_the_first_waiter() {
        let state = ClarifyState::new();
        let rx1 = state.register("s1");
        let rx2 = state.register("s1");
        assert!(rx1.await.is_err(), "superseded waiter reads no answer");
        assert!(state.resolve("s1", "answer"));
        assert_eq!(rx2.await.unwrap(), "answer");
    }
}
