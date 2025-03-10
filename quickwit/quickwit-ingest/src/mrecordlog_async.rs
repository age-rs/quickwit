// Copyright (C) 2024 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::io;
use std::ops::RangeBounds;
use std::path::Path;

use bytes::Buf;
use mrecordlog::error::*;
use mrecordlog::{MultiRecordLog, Record, SyncPolicy};
use tokio::task::JoinError;
use tracing::error;

/// A light wrapper to allow async operation in mrecordlog.
pub struct MultiRecordLogAsync {
    mrecordlog_opt: Option<MultiRecordLog>,
}

impl MultiRecordLogAsync {
    fn take(&mut self) -> MultiRecordLog {
        let Some(mrecordlog) = self.mrecordlog_opt.take() else {
            error!("wal is poisoned, aborting process");
            std::process::abort();
        };
        mrecordlog
    }

    fn mrecordlog_ref(&self) -> &MultiRecordLog {
        let Some(mrecordlog) = &self.mrecordlog_opt else {
            error!("the mrecordlog is corrupted, aborting process");
            std::process::abort();
        };
        mrecordlog
    }

    pub async fn open(directory_path: &Path) -> Result<Self, ReadRecordError> {
        Self::open_with_prefs(directory_path, SyncPolicy::OnAppend).await
    }

    pub async fn open_with_prefs(
        directory_path: &Path,
        sync_policy: SyncPolicy,
    ) -> Result<Self, ReadRecordError> {
        let directory_path = directory_path.to_path_buf();
        let mrecordlog = tokio::task::spawn(async move {
            MultiRecordLog::open_with_prefs(&directory_path, sync_policy)
        })
        .await
        .map_err(|join_err| {
            error!(error=?join_err, "failed to load WAL");
            ReadRecordError::IoError(io::Error::new(
                io::ErrorKind::Other,
                "loading wal from directory failed",
            ))
        })??;
        Ok(Self {
            mrecordlog_opt: Some(mrecordlog),
        })
    }

    async fn run_operation<F, T>(&mut self, operation: F) -> T
    where
        F: FnOnce(&mut MultiRecordLog) -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut mrecordlog = self.take();
        let join_res: Result<(T, MultiRecordLog), JoinError> =
            tokio::task::spawn_blocking(move || {
                let res = operation(&mut mrecordlog);
                (res, mrecordlog)
            })
            .await;
        match join_res {
            Ok((operation_result, mrecordlog)) => {
                self.mrecordlog_opt = Some(mrecordlog);
                operation_result
            }
            Err(join_error) => {
                // This could be caused by a panic
                error!(error=?join_error, "failed to run mrecordlog operation");
                panic!("failed to run mrecordlog operation");
            }
        }
    }

    pub async fn create_queue(&mut self, queue: &str) -> Result<(), CreateQueueError> {
        let queue = queue.to_string();
        self.run_operation(move |mrecordlog| mrecordlog.create_queue(&queue))
            .await
    }

    pub async fn delete_queue(&mut self, queue: &str) -> Result<(), DeleteQueueError> {
        let queue = queue.to_string();
        self.run_operation(move |mrecordlog| mrecordlog.delete_queue(&queue))
            .await
    }

    pub async fn append_records<T: Iterator<Item = impl Buf> + Send + 'static>(
        &mut self,
        queue: &str,
        position_opt: Option<u64>,
        payloads: T,
    ) -> Result<Option<u64>, AppendError> {
        let queue = queue.to_string();
        self.run_operation(move |mrecordlog| {
            mrecordlog.append_records(&queue, position_opt, payloads)
        })
        .await
    }

    #[track_caller]
    #[cfg(test)]
    pub fn assert_records_eq<R>(&self, queue_id: &str, range: R, expected_records: &[(u64, &str)])
    where R: RangeBounds<u64> + 'static {
        let records = self
            .range(queue_id, range)
            .unwrap()
            .map(|Record { position, payload }| {
                (position, String::from_utf8(payload.into_owned()).unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            records.len(),
            expected_records.len(),
            "expected {} records, got {}",
            expected_records.len(),
            records.len()
        );
        for ((position, record), (expected_position, expected_record)) in
            records.iter().zip(expected_records.iter())
        {
            assert_eq!(
                position, expected_position,
                "expected record at position `{expected_position}`, got `{position}`",
            );
            assert_eq!(
                record, expected_record,
                "expected record `{expected_record}`, got `{record}`",
            );
        }
    }

    pub async fn truncate(&mut self, queue: &str, position: u64) -> Result<usize, TruncateError> {
        let queue = queue.to_string();
        self.run_operation(move |mrecordlog| mrecordlog.truncate(&queue, position))
            .await
    }

    pub fn range<R>(
        &self,
        queue: &str,
        range: R,
    ) -> Result<impl Iterator<Item = Record<'_>> + '_, MissingQueue>
    where
        R: RangeBounds<u64> + 'static,
    {
        self.mrecordlog_ref().range(queue, range)
    }

    pub fn queue_exists(&self, queue: &str) -> bool {
        self.mrecordlog_ref().queue_exists(queue)
    }

    pub fn list_queues(&self) -> impl Iterator<Item = &str> {
        self.mrecordlog_ref().list_queues()
    }

    pub fn last_record(&self, queue: &str) -> Result<Option<Record<'_>>, MissingQueue> {
        self.mrecordlog_ref().last_record(queue)
    }

    pub fn memory_usage(&self) -> usize {
        self.mrecordlog_ref().memory_usage()
    }

    pub fn disk_usage(&self) -> usize {
        self.mrecordlog_ref().disk_usage()
    }
}
