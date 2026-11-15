//! The request log: every site request is recorded, buffered in memory and written to
//! SQLite in batches so that logging never slows down serving.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};

use crate::visitor::UaClass;

/// How many records may wait in memory. Beyond this, new records are dropped (and
/// counted) rather than blocking a request.
const QUEUE_CAPACITY: usize = 10_000;
/// Write a batch at this size...
const BATCH_SIZE: usize = 500;
/// ...or after this long, whichever comes first.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// Longest stored request path.
pub const MAX_PATH_CHARS: usize = 512;

/// One served request. Contains no IP address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// Unix milliseconds.
    pub ts: i64,
    pub site_id: i64,
    pub method: String,
    /// The path without its query string.
    pub path: String,
    pub status: u16,
    pub bytes: u64,
    pub duration_ms: u64,
    pub referrer_host: Option<String>,
    pub ua_class: UaClass,
    pub visitor_hash: String,
}

enum Message {
    Record(Box<RequestRecord>),
    Flush(oneshot::Sender<()>),
}

/// Handle for recording requests. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RequestLog {
    tx: mpsc::Sender<Message>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Record(_) => f.write_str("Record"),
            Message::Flush(_) => f.write_str("Flush"),
        }
    }
}

impl RequestLog {
    /// Start the background writer. It runs until every `RequestLog` handle is dropped,
    /// then writes what is left and finishes; await the returned task to wait for that.
    pub fn spawn(db: SqlitePool) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let task = tokio::spawn(writer(db, rx));
        let log = Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        (log, task)
    }

    /// Queue a record. Never blocks: if the queue is full the record is dropped.
    pub fn record(&self, mut record: RequestRecord) {
        if record.path.chars().count() > MAX_PATH_CHARS {
            record.path = record.path.chars().take(MAX_PATH_CHARS).collect();
        }
        if self.tx.try_send(Message::Record(Box::new(record))).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                tracing::warn!(dropped, "request log queue is full; dropping records");
            }
        }
    }

    /// Records dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Wait until everything queued so far has been written.
    pub async fn flush(&self) {
        let (ack, done) = oneshot::channel();
        if self.tx.send(Message::Flush(ack)).await.is_ok() {
            let _ = done.await;
        }
    }
}

async fn writer(db: SqlitePool, mut rx: mpsc::Receiver<Message>) {
    let mut batch: Vec<RequestRecord> = Vec::with_capacity(BATCH_SIZE);
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            message = rx.recv() => match message {
                Some(Message::Record(record)) => {
                    batch.push(*record);
                    if batch.len() >= BATCH_SIZE {
                        write_batch(&db, &mut batch).await;
                    }
                }
                Some(Message::Flush(ack)) => {
                    write_batch(&db, &mut batch).await;
                    let _ = ack.send(());
                }
                None => {
                    write_batch(&db, &mut batch).await;
                    return;
                }
            },
            _ = ticker.tick() => write_batch(&db, &mut batch).await,
        }
    }
}

async fn write_batch(db: &SqlitePool, batch: &mut Vec<RequestRecord>) {
    if batch.is_empty() {
        return;
    }
    let result: Result<(), sqlx::Error> = async {
        let mut tx = db.begin().await?;
        for r in batch.iter() {
            let (status, bytes, duration) = (i64::from(r.status), r.bytes as i64, r.duration_ms as i64);
            let ua_class = r.ua_class.as_str();
            sqlx::query!(
                "INSERT INTO requests (ts, site_id, method, path, status, bytes, duration_ms, referrer_host, ua_class, visitor_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                r.ts,
                r.site_id,
                r.method,
                r.path,
                status,
                bytes,
                duration,
                r.referrer_host,
                ua_class,
                r.visitor_hash
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::error!(%error, records = batch.len(), "could not write request log batch");
    }
    batch.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str) -> RequestRecord {
        RequestRecord {
            ts: 1,
            site_id: 7,
            method: "GET".into(),
            path: path.into(),
            status: 200,
            bytes: 12,
            duration_ms: 3,
            referrer_host: Some("example.org".into()),
            ua_class: UaClass::Browser,
            visitor_hash: "abc".into(),
        }
    }

    #[tokio::test]
    async fn records_are_written_in_batches_and_on_shutdown() {
        let db = sw_core::db::open_in_memory().await.unwrap();
        let (log, task) = RequestLog::spawn(db.clone());
        for i in 0..1200 {
            log.record(record(&format!("/p{i}")));
        }
        log.flush().await;
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM requests")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1200);

        log.record(record(&"x".repeat(2000)));
        drop(log);
        task.await.unwrap();
        let (n, longest): (i64, i64) =
            sqlx::query_as("SELECT count(*), max(length(path)) FROM requests")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 1201);
        assert_eq!(longest, MAX_PATH_CHARS as i64);
    }
}
