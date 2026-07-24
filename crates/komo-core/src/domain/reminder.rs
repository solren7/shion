use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Reminder {
    pub id: String,
    pub message: String,
    pub run_at: i64,
    pub status: ReminderStatus,
    /// 5-field cron expression (local timezone). Empty string = one-shot reminder.
    pub schedule: String,
    pub created_at: i64,
}

impl Reminder {
    pub fn new(message: String, run_at: i64) -> Self {
        Self {
            id: format!("rem-{}", uuid::Uuid::now_v7()),
            message,
            run_at,
            status: ReminderStatus::Pending,
            schedule: String::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }

    pub fn recurring(message: String, run_at: i64, schedule: String) -> Self {
        Self {
            id: format!("rem-{}", uuid::Uuid::now_v7()),
            message,
            run_at,
            status: ReminderStatus::Pending,
            schedule,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }

    pub fn is_recurring(&self) -> bool {
        !self.schedule.is_empty()
    }
}

#[async_trait]
pub trait ReminderRepository: Send + Sync {
    async fn save(&self, reminder: &Reminder) -> anyhow::Result<()>;
    /// Return all reminders with status = Pending.
    async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>>;
    /// Transition a reminder's status (Pending → Fired / Missed / Cancelled).
    async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()>;
    /// Advance a recurring reminder to its next occurrence (status stays Pending).
    async fn reschedule(&self, id: &str, next_run_at: i64) -> anyhow::Result<()>;
}
