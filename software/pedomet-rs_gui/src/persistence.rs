use std::time::Duration;

use anyhow::anyhow;
use app_dirs2::{app_root, AppDataType};
use log::{info, warn};
use pedomet_rs_common::{PedometerEvent, PedometerEventType};
use sqlx::{prelude::FromRow, SqlitePool};
use time::{OffsetDateTime, UtcOffset};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{error::PedometerGuiError, APP_INFO};

#[derive(Debug, Copy, Clone, FromRow)]
pub(crate) struct PedometerPersistenceEvent {
    pub event_id: i64,
    pub timestamp_ms: i64,
    pub boot_id: i64,
    pub steps: i64,
}

impl PedometerPersistenceEvent {
    pub fn from_common_event(
        common_event: PedometerEvent,
        offset: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            event_id: common_event.index as i64,
            timestamp_ms: (common_event.timestamp_ms + offset.as_millis() as u64).try_into()?,
            boot_id: common_event.boot_id as i64,
            steps: if let PedometerEventType::Steps(steps) = common_event.event_type {
                steps as i64
            } else {
                return Err(PedometerGuiError::InvalidEventType(common_event.event_type).into());
            },
        })
    }

    pub fn get_date_time(&self) -> anyhow::Result<OffsetDateTime> {
        Ok(OffsetDateTime::from_unix_timestamp_nanos(
            self.event_id as i128 * 1000 * 1000,
        )?)
    }
}

pub(crate) struct PedometerDatabase {
    pool: SqlitePool,
}

impl PedometerDatabase {
    pub(crate) async fn new() -> anyhow::Result<Self> {
        let mut db_file = app_root(AppDataType::UserData, &APP_INFO)?;
        db_file.push("events.db");
        info!("Database file: {:?}", db_file);
        let pool =
            SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db_file.to_string_lossy())).await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
    pub(crate) async fn spawn_message_handler(
        self,
        mut event_receiver: mpsc::Receiver<PedometerDatabaseCommand>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = event_receiver.recv().await {
                match cmd {
                    PedometerDatabaseCommand::AddEvent { event, responder } => {
                        info!("Got AddEvent command: {event:?}");
                        if responder.send(self.add_event(event).await).is_err() {
                            warn!("Could not send response");
                        }
                    }
                    PedometerDatabaseCommand::GetEventsInTimeRange {
                        start,
                        end,
                        responder,
                    } => {
                        if responder
                            .send(self.get_events_in_time_range(start, end).await)
                            .is_err()
                        {
                            warn!("Could not send response");
                        }
                    }
                    PedometerDatabaseCommand::Exit => break,
                }
            }
        })
    }
    async fn add_event(&self, event: PedometerPersistenceEvent) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query!(
            "
        INSERT INTO events ( event_id, timestamp_ms, boot_id, steps  )
        VALUES ( ?, ?, ?, ? )
        ",
            event.event_id,
            event.timestamp_ms,
            event.boot_id,
            event.steps,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn get_events_in_time_range(
        &self,
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> anyhow::Result<Vec<PedometerPersistenceEvent>> {
        let start_ms: i64 =
            (start.to_offset(UtcOffset::UTC).unix_timestamp_nanos() / 1000 / 1000).try_into()?;
        let end_ms: i64 =
            (end.to_offset(UtcOffset::UTC).unix_timestamp_nanos() / 1000 / 1000).try_into()?;
        Ok(sqlx::query_as!(
            PedometerPersistenceEvent,
            "
        SELECT event_id, timestamp_ms, boot_id, steps
        FROM events
        WHERE timestamp_ms BETWEEN ? AND ?
        ",
            start_ms,
            end_ms,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get_max_event_id(&self) -> anyhow::Result<u32> {
        Ok(sqlx::query!(
            "
        SELECT max(event_id) as max_event_id
        FROM events
        "
        )
        .fetch_one(&self.pool)
        .await?
        .max_event_id
        .ok_or_else(|| anyhow!("No events in database"))?
        .try_into()?)
    }
}

pub(crate) enum PedometerDatabaseCommand {
    AddEvent {
        event: PedometerPersistenceEvent,
        responder: oneshot::Sender<anyhow::Result<()>>,
    },
    GetEventsInTimeRange {
        start: OffsetDateTime,
        end: OffsetDateTime,
        responder: oneshot::Sender<anyhow::Result<Vec<PedometerPersistenceEvent>>>,
    },
    Exit,
}

pub(crate) type PedometerDatabaseCommandSender = mpsc::Sender<PedometerDatabaseCommand>;
pub(crate) type PedometerDatabaseAddEventResult = ();
pub(crate) type PedometerDatabaseGetEventsInTimeRangeReceiver =
    anyhow::Result<Vec<PedometerPersistenceEvent>>;
