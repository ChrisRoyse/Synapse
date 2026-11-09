use std::{
    sync::{Arc, mpsc, mpsc::Sender},
    thread,
    thread::JoinHandle,
    time::Duration,
};

use rocksdb::{DB, WriteBatch, WriteOptions};

use crate::{StorageError, StorageResult};

pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const STORAGE_WRITE_BATCH_FLUSHES_TOTAL: &str = "storage_write_batch_flushes_total";

pub struct Batcher {
    sender: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Batcher {
    pub fn spawn(db: Arc<DB>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || Worker { db, receiver }.run());
        Self {
            sender,
            worker: Some(worker),
        }
    }

    pub fn put_batch(&self, cf_name: &str, kvs: Vec<(Vec<u8>, Vec<u8>)>) -> StorageResult<()> {
        let (reply, wait) = mpsc::sync_channel(1);
        self.sender
            .send(Command::Write {
                cf_name: cf_name.to_owned(),
                kvs,
                reply,
            })
            .map_err(|error| write_failed(cf_name, error.to_string()))?;
        wait.recv()
            .map_err(|error| write_failed(cf_name, error.to_string()))?
    }

    pub fn flush(&self) -> StorageResult<()> {
        let (reply, wait) = mpsc::sync_channel(1);
        self.sender
            .send(Command::Flush { reply })
            .map_err(|error| write_failed("batcher", error.to_string()))?;
        wait.recv()
            .map_err(|error| write_failed("batcher", error.to_string()))?
    }
}

impl Drop for Batcher {
    fn drop(&mut self) {
        let (reply, wait) = mpsc::sync_channel(1);
        let _ = self.sender.send(Command::Shutdown { reply });
        let _ = wait.recv_timeout(FLUSH_INTERVAL.saturating_mul(2));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Command {
    Write {
        cf_name: String,
        kvs: Vec<(Vec<u8>, Vec<u8>)>,
        reply: mpsc::SyncSender<StorageResult<()>>,
    },
    Flush {
        reply: mpsc::SyncSender<StorageResult<()>>,
    },
    Shutdown {
        reply: mpsc::SyncSender<StorageResult<()>>,
    },
}

struct Worker {
    db: Arc<DB>,
    receiver: mpsc::Receiver<Command>,
}

impl Worker {
    fn run(self) {
        loop {
            match self.receiver.recv() {
                Ok(Command::Write {
                    cf_name,
                    kvs,
                    reply,
                }) => {
                    let result = write_batch(&self.db, &cf_name, kvs, "write");
                    let _ = reply.send(result);
                }
                Ok(Command::Flush { reply }) => {
                    let result = sync_wal(&self.db, "explicit");
                    let _ = reply.send(result);
                }
                Ok(Command::Shutdown { reply }) => {
                    let result = sync_wal(&self.db, "shutdown");
                    let _ = reply.send(result);
                    break;
                }
                Err(_disconnected) => {
                    if let Err(error) = sync_wal(&self.db, "disconnected") {
                        tracing::warn!(
                            error = %error,
                            "storage batcher final WAL sync failed after command channel disconnected"
                        );
                    }
                    break;
                }
            }
        }
    }
}

fn write_batch(
    db: &DB,
    cf_name: &str,
    kvs: Vec<(Vec<u8>, Vec<u8>)>,
    trigger: &'static str,
) -> StorageResult<()> {
    if kvs.is_empty() {
        return sync_wal(db, trigger);
    }

    let cf = db.cf_handle(cf_name).ok_or_else(|| {
        write_failed(
            cf_name,
            "column family handle missing while writing batch".to_owned(),
        )
    })?;
    let mut batch = WriteBatch::default();
    for (key, value) in kvs {
        batch.put_cf(&cf, key, value);
    }

    let mut options = WriteOptions::default();
    options.set_sync(true);
    db.write_opt(batch, &options)
        .map_err(|error| write_failed(cf_name, error.to_string()))?;
    db.flush_wal(true)
        .map_err(|error| write_failed(cf_name, error.to_string()))?;
    synapse_telemetry::metrics::counter!(STORAGE_WRITE_BATCH_FLUSHES_TOTAL, "trigger" => trigger)
        .increment(1);
    Ok(())
}

fn sync_wal(db: &DB, trigger: &'static str) -> StorageResult<()> {
    db.flush_wal(true)
        .map_err(|error| write_failed("batcher", error.to_string()))?;
    synapse_telemetry::metrics::counter!(STORAGE_WRITE_BATCH_FLUSHES_TOTAL, "trigger" => trigger)
        .increment(1);
    Ok(())
}

fn write_failed(cf_name: &str, detail: String) -> StorageError {
    StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail,
    }
}
