use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderStatus {
    Pending,
    Fired,
    Missed,
    Cancelled,
}

impl ReminderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fired => "fired",
            Self::Missed => "missed",
            Self::Cancelled => "cancelled",
        }
    }
}

pub fn parse_reminder_status(s: &str) -> ReminderStatus {
    match s {
        "fired" => ReminderStatus::Fired,
        "missed" => ReminderStatus::Missed,
        "cancelled" => ReminderStatus::Cancelled,
        _ => ReminderStatus::Pending,
    }
}

#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: String,
    pub message: String,
    pub run_at: i64,
    pub status: ReminderStatus,
    pub created_at: i64,
}

impl Reminder {
    pub fn new(message: String, run_at: i64) -> Self {
        Self {
            id: format!(
                "rem-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            message,
            run_at,
            status: ReminderStatus::Pending,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }
}

#[async_trait]
pub trait ReminderRepository: Send + Sync {
    async fn save(&self, reminder: &Reminder) -> anyhow::Result<()>;
    /// Return all reminders with status = Pending.
    async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>>;
    /// Transition a reminder's status (Pending → Fired / Missed / Cancelled).
    async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()>;
}
