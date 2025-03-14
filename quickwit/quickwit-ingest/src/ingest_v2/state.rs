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

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use fnv::FnvHashMap;
use mrecordlog::error::{DeleteQueueError, TruncateError};
use quickwit_common::pretty::PrettyDisplay;
use quickwit_common::rate_limiter::{RateLimiter, RateLimiterSettings};
use quickwit_proto::control_plane::AdviseResetShardsResponse;
use quickwit_proto::ingest::ingester::IngesterStatus;
use quickwit_proto::ingest::{IngestV2Error, IngestV2Result, ShardState};
use quickwit_proto::types::{Position, QueueId};
use tokio::sync::{watch, Mutex, MutexGuard, RwLock, RwLockMappedWriteGuard, RwLockWriteGuard};
use tracing::{error, info};

use super::models::IngesterShard;
use super::rate_meter::RateMeter;
use super::replication::{ReplicationStreamTaskHandle, ReplicationTaskHandle};
use crate::ingest_v2::mrecordlog_utils::{force_delete_queue, queue_position_range};
use crate::mrecordlog_async::MultiRecordLogAsync;
use crate::{FollowerId, LeaderId};

/// Stores the state of the ingester and attempts to prevent deadlocks by exposing an API that
/// guarantees that the internal data structures are always locked in the same order.
///
/// `lock_partially` locks `inner` only, while `lock_fully` locks both `inner` and `mrecordlog`. Use
/// the former when you only need to access the in-memory state of the ingester and the latter when
/// you need to access both the in-memory state AND the WAL.
#[derive(Clone)]
pub(super) struct IngesterState {
    // `inner` is a mutex because it's almost always accessed mutably.
    inner: Arc<Mutex<InnerIngesterState>>,
    mrecordlog: Arc<RwLock<Option<MultiRecordLogAsync>>>,
    pub status_rx: watch::Receiver<IngesterStatus>,
}

pub(super) struct InnerIngesterState {
    pub shards: FnvHashMap<QueueId, IngesterShard>,
    pub rate_trackers: FnvHashMap<QueueId, (RateLimiter, RateMeter)>,
    // Replication stream opened with followers.
    pub replication_streams: FnvHashMap<FollowerId, ReplicationStreamTaskHandle>,
    // Replication tasks running for each replication stream opened with leaders.
    pub replication_tasks: FnvHashMap<LeaderId, ReplicationTaskHandle>,
    status: IngesterStatus,
    status_tx: watch::Sender<IngesterStatus>,
}

impl InnerIngesterState {
    pub fn status(&self) -> IngesterStatus {
        self.status
    }

    pub fn set_status(&mut self, status: IngesterStatus) {
        self.status = status;
        self.status_tx.send(status).expect("channel should be open");
    }
}

impl IngesterState {
    fn new() -> Self {
        let status = IngesterStatus::Initializing;
        let (status_tx, status_rx) = watch::channel(status);
        let inner = InnerIngesterState {
            shards: Default::default(),
            rate_trackers: Default::default(),
            replication_streams: Default::default(),
            replication_tasks: Default::default(),
            status,
            status_tx,
        };
        let inner = Arc::new(Mutex::new(inner));
        let mrecordlog = Arc::new(RwLock::new(None));

        Self {
            inner,
            mrecordlog,
            status_rx,
        }
    }

    pub fn load(wal_dir_path: &Path, rate_limiter_settings: RateLimiterSettings) -> Self {
        let state = Self::new();
        let state_clone = state.clone();
        let wal_dir_path = wal_dir_path.to_path_buf();

        let init_future = async move {
            state_clone.init(&wal_dir_path, rate_limiter_settings).await;
        };
        tokio::spawn(init_future);

        state
    }

    #[cfg(test)]
    pub async fn for_test() -> (tempfile::TempDir, Self) {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut state = IngesterState::load(temp_dir.path(), RateLimiterSettings::default());

        state
            .status_rx
            .wait_for(|status| *status == IngesterStatus::Ready)
            .await
            .unwrap();

        (temp_dir, state)
    }

    /// Initializes the internal state of the ingester. It loads the local WAL, then lists all its
    /// queues. Empty queues are deleted, while non-empty queues are recovered. However, the
    /// corresponding shards are closed and become read-only.
    pub async fn init(&self, wal_dir_path: &Path, rate_limiter_settings: RateLimiterSettings) {
        let mut inner_guard = self.inner.lock().await;
        let mut mrecordlog_guard = self.mrecordlog.write().await;

        let now = Instant::now();

        info!("opening WAL located at `{}`", wal_dir_path.display());
        let open_result = MultiRecordLogAsync::open_with_prefs(
            wal_dir_path,
            mrecordlog::SyncPolicy::OnDelay(Duration::from_secs(5)),
        )
        .await;

        let mut mrecordlog = match open_result {
            Ok(mrecordlog) => {
                info!(
                    "opened WAL successfully in {}",
                    now.elapsed().pretty_display()
                );
                mrecordlog
            }
            Err(error) => {
                error!("failed to open WAL: {error}");
                inner_guard.set_status(IngesterStatus::Failed);
                return;
            }
        };
        let queue_ids: Vec<QueueId> = mrecordlog
            .list_queues()
            .map(|queue_id| queue_id.to_string())
            .collect();

        if !queue_ids.is_empty() {
            info!("recovering {} shard(s)", queue_ids.len());
        }
        let now = Instant::now();
        let mut num_closed_shards = 0;
        let mut num_deleted_shards = 0;

        for queue_id in queue_ids {
            if let Some(position_range) = queue_position_range(&mrecordlog, &queue_id) {
                // The queue is not empty: recover it.
                let replication_position_inclusive = Position::offset(*position_range.end());
                let truncation_position_inclusive = if *position_range.start() == 0 {
                    Position::Beginning
                } else {
                    Position::offset(*position_range.start() - 1)
                };
                let solo_shard = IngesterShard::new_solo(
                    ShardState::Closed,
                    replication_position_inclusive,
                    truncation_position_inclusive,
                    now,
                );
                inner_guard.shards.insert(queue_id.clone(), solo_shard);

                let rate_limiter = RateLimiter::from_settings(rate_limiter_settings);
                let rate_meter = RateMeter::default();
                inner_guard
                    .rate_trackers
                    .insert(queue_id, (rate_limiter, rate_meter));

                num_closed_shards += 1;
            } else {
                // The queue is empty: delete it.
                if let Err(io_error) = force_delete_queue(&mut mrecordlog, &queue_id).await {
                    error!("failed to delete shard `{queue_id}`: {io_error}");
                    continue;
                }
                num_deleted_shards += 1;
            }
        }
        if num_closed_shards > 0 {
            info!("recovered and closed {num_closed_shards} shard(s)");
        }
        if num_deleted_shards > 0 {
            info!("deleted {num_deleted_shards} empty shard(s)");
        }
        mrecordlog_guard.replace(mrecordlog);
        inner_guard.set_status(IngesterStatus::Ready);
    }

    pub async fn wait_for_ready(&mut self) {
        self.status_rx
            .wait_for(|status| *status == IngesterStatus::Ready)
            .await
            .expect("channel should be open");
    }

    pub async fn lock_partially(&self) -> IngestV2Result<PartiallyLockedIngesterState<'_>> {
        if *self.status_rx.borrow() == IngesterStatus::Initializing {
            return Err(IngestV2Error::Internal(
                "ingester is initializing".to_string(),
            ));
        }
        let inner_guard = self.inner.lock().await;

        if inner_guard.status() == IngesterStatus::Failed {
            return Err(IngestV2Error::Internal(
                "failed to initialize ingester".to_string(),
            ));
        }
        let partial_lock = PartiallyLockedIngesterState { inner: inner_guard };
        Ok(partial_lock)
    }

    pub async fn lock_fully(&self) -> IngestV2Result<FullyLockedIngesterState<'_>> {
        if *self.status_rx.borrow() == IngesterStatus::Initializing {
            return Err(IngestV2Error::Internal(
                "ingester is initializing".to_string(),
            ));
        }
        // We assume that the mrecordlog lock is the most "expensive" one to acquire, so we acquire
        // it first.
        let mrecordlog_opt_guard = self.mrecordlog.write().await;
        let inner_guard = self.inner.lock().await;

        if inner_guard.status() == IngesterStatus::Failed {
            return Err(IngestV2Error::Internal(
                "failed to initialize ingester".to_string(),
            ));
        }
        let mrecordlog_guard = RwLockWriteGuard::map(mrecordlog_opt_guard, |mrecordlog_opt| {
            mrecordlog_opt
                .as_mut()
                .expect("mrecordlog should be initialized")
        });
        let full_lock = FullyLockedIngesterState {
            inner: inner_guard,
            mrecordlog: mrecordlog_guard,
        };
        Ok(full_lock)
    }

    // Leaks the mrecordlog lock for use in fetch tasks. It's safe to do so because fetch tasks
    // never attempt to lock the inner state.
    pub fn mrecordlog(&self) -> Arc<RwLock<Option<MultiRecordLogAsync>>> {
        self.mrecordlog.clone()
    }

    pub fn weak(&self) -> WeakIngesterState {
        WeakIngesterState {
            inner: Arc::downgrade(&self.inner),
            mrecordlog: Arc::downgrade(&self.mrecordlog),
            status_rx: self.status_rx.clone(),
        }
    }
}

pub(super) struct PartiallyLockedIngesterState<'a> {
    pub inner: MutexGuard<'a, InnerIngesterState>,
}

impl fmt::Debug for PartiallyLockedIngesterState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("PartiallyLockedIngesterState").finish()
    }
}

impl Deref for PartiallyLockedIngesterState<'_> {
    type Target = InnerIngesterState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for PartiallyLockedIngesterState<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub(super) struct FullyLockedIngesterState<'a> {
    pub inner: MutexGuard<'a, InnerIngesterState>,
    pub mrecordlog: RwLockMappedWriteGuard<'a, MultiRecordLogAsync>,
}

impl fmt::Debug for FullyLockedIngesterState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FullyLockedIngesterState").finish()
    }
}

impl Deref for FullyLockedIngesterState<'_> {
    type Target = InnerIngesterState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for FullyLockedIngesterState<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl FullyLockedIngesterState<'_> {
    /// Deletes the shard identified by `queue_id` from the ingester state. It removes the
    /// mrecordlog queue first and then removes the associated in-memory shard and rate trackers.
    pub async fn delete_shard(&mut self, queue_id: &QueueId) {
        match self.mrecordlog.delete_queue(queue_id).await {
            Ok(_) | Err(DeleteQueueError::MissingQueue(_)) => {
                self.rate_trackers.remove(queue_id);

                // Log only if the shard was actually removed.
                if self.shards.remove(queue_id).is_some() {
                    info!("deleted shard `{queue_id}`");
                }
            }
            Err(DeleteQueueError::IoError(io_error)) => {
                error!("failed to delete shard `{queue_id}`: {io_error}");
            }
        };
    }

    /// Truncates the shard identified by `queue_id` up to `truncate_up_to_position_inclusive` only
    /// if the current truncation position of the shard is smaller.
    pub async fn truncate_shard(
        &mut self,
        queue_id: &QueueId,
        truncate_up_to_position_inclusive: &Position,
    ) {
        // TODO: Replace with if-let-chains when stabilized.
        let Some(truncate_up_to_offset_inclusive) = truncate_up_to_position_inclusive.as_u64()
        else {
            return;
        };
        let Some(shard) = self.inner.shards.get_mut(queue_id) else {
            return;
        };
        if shard.truncation_position_inclusive >= *truncate_up_to_position_inclusive {
            return;
        }
        match self
            .mrecordlog
            .truncate(queue_id, truncate_up_to_offset_inclusive)
            .await
        {
            Ok(_) => {
                shard.truncation_position_inclusive = truncate_up_to_position_inclusive.clone();
            }
            Err(TruncateError::MissingQueue(_)) => {
                error!("failed to truncate shard `{queue_id}`: WAL queue not found");
                self.shards.remove(queue_id);
                self.rate_trackers.remove(queue_id);
                info!("deleted dangling shard `{queue_id}`");
            }
            Err(TruncateError::IoError(io_error)) => {
                error!("failed to truncate shard `{queue_id}`: {io_error}");
            }
        };
    }

    /// Deletes and truncates the shards as directed by the `advise_reset_shards_response` returned
    /// by the control plane.
    pub async fn reset_shards(&mut self, advise_reset_shards_response: &AdviseResetShardsResponse) {
        for shard_ids in &advise_reset_shards_response.shards_to_delete {
            for queue_id in shard_ids.queue_ids() {
                self.delete_shard(&queue_id).await;
            }
        }
        for shard_id_positions in &advise_reset_shards_response.shards_to_truncate {
            for (queue_id, publish_position) in shard_id_positions.queue_id_positions() {
                self.truncate_shard(&queue_id, publish_position).await;
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct WeakIngesterState {
    inner: Weak<Mutex<InnerIngesterState>>,
    mrecordlog: Weak<RwLock<Option<MultiRecordLogAsync>>>,
    status_rx: watch::Receiver<IngesterStatus>,
}

impl WeakIngesterState {
    pub fn upgrade(&self) -> Option<IngesterState> {
        let inner = self.inner.upgrade()?;
        let mrecordlog = self.mrecordlog.upgrade()?;
        let status_rx = self.status_rx.clone();
        let state = IngesterState {
            inner,
            mrecordlog,
            status_rx,
        };
        Some(state)
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn test_ingester_state_does_not_lock_while_initializing() {
        let state = IngesterState::new();
        let inner_guard = state.inner.lock().await;

        assert_eq!(inner_guard.status(), IngesterStatus::Initializing);
        assert_eq!(*state.status_rx.borrow(), IngesterStatus::Initializing);

        let error = state.lock_partially().await.unwrap_err().to_string();
        assert!(error.contains("ingester is initializing"));

        let error = state.lock_fully().await.unwrap_err().to_string();
        assert!(error.contains("ingester is initializing"));
    }

    #[tokio::test]
    async fn test_ingester_state_failed() {
        let state = IngesterState::new();

        state.inner.lock().await.set_status(IngesterStatus::Failed);

        let error = state.lock_partially().await.unwrap_err().to_string();
        assert!(error.to_string().ends_with("failed to initialize ingester"));

        let error = state.lock_fully().await.unwrap_err().to_string();
        assert!(error.contains("failed to initialize ingester"));
    }

    #[tokio::test]
    async fn test_ingester_state_init() {
        let mut state = IngesterState::new();
        let temp_dir = tempfile::tempdir().unwrap();

        state
            .init(temp_dir.path(), RateLimiterSettings::default())
            .await;

        timeout(Duration::from_millis(100), state.wait_for_ready())
            .await
            .unwrap();

        state.lock_partially().await.unwrap();

        let locked_state = state.lock_fully().await.unwrap();
        assert_eq!(locked_state.status(), IngesterStatus::Ready);
        assert_eq!(*locked_state.status_tx.borrow(), IngesterStatus::Ready);
    }
}
