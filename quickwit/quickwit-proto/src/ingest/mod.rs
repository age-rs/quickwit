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

use bytes::Bytes;

use self::ingester::{PersistFailureReason, ReplicateFailureReason};
use self::router::IngestFailureReason;
use super::types::NodeId;
use super::{ServiceError, ServiceErrorCode};
use crate::control_plane::ControlPlaneError;
use crate::types::{queue_id, IndexUid, Position, QueueId, ShardId};

pub mod ingester;
pub mod router;

include!("../codegen/quickwit/quickwit.ingest.rs");

pub type IngestV2Result<T> = std::result::Result<T, IngestV2Error>;

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum IngestV2Error {
    #[error("an internal error occurred: {0}")]
    Internal(String),
    /// Emitted when an ingester was not available for a given operation,
    /// either directly or through replication.
    #[error("failed to connect to ingester `{ingester_id}`")]
    IngesterUnavailable { ingester_id: NodeId },
    #[error("shard `{shard_id}` not found")]
    ShardNotFound { shard_id: ShardId },
    #[error("request timed out")]
    Timeout,
    // This error is provoked by semaphore located on the router.
    #[error("too many requests")]
    TooManyRequests,
    // TODO: Merge `Transport` and `IngesterUnavailable` into a single `Unavailable` error.
    #[error("transport error: {0}")]
    Transport(String),
}

impl IngestV2Error {
    pub fn label_value(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "timeout",
            _ => "error",
        }
    }
}

impl From<ControlPlaneError> for IngestV2Error {
    fn from(error: ControlPlaneError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<IngestV2Error> for tonic::Status {
    fn from(error: IngestV2Error) -> tonic::Status {
        let code = match &error {
            IngestV2Error::IngesterUnavailable { .. } => tonic::Code::Unavailable,
            IngestV2Error::Internal(_) => tonic::Code::Internal,
            IngestV2Error::ShardNotFound { .. } => tonic::Code::NotFound,
            IngestV2Error::Timeout { .. } => tonic::Code::DeadlineExceeded,
            IngestV2Error::TooManyRequests => tonic::Code::ResourceExhausted,
            IngestV2Error::Transport { .. } => tonic::Code::Unavailable,
        };
        let error_json: String = serde_json::to_string(&error).unwrap();
        tonic::Status::new(code, error_json)
    }
}

impl From<tonic::Status> for IngestV2Error {
    fn from(status: tonic::Status) -> Self {
        if let Ok(error_from_json) = serde_json::from_str(status.message()) {
            return error_from_json;
        }
        match status.code() {
            tonic::Code::Unavailable => IngestV2Error::Transport(status.message().to_string()),
            tonic::Code::ResourceExhausted => IngestV2Error::TooManyRequests,
            _ => IngestV2Error::Internal(status.message().to_string()),
        }
    }
}

impl ServiceError for IngestV2Error {
    fn error_code(&self) -> ServiceErrorCode {
        match self {
            Self::IngesterUnavailable { .. } => ServiceErrorCode::Unavailable,
            Self::Internal { .. } => ServiceErrorCode::Internal,
            Self::ShardNotFound { .. } => ServiceErrorCode::NotFound,
            Self::Timeout { .. } => ServiceErrorCode::Timeout,
            Self::Transport { .. } => ServiceErrorCode::Unavailable,
            Self::TooManyRequests => ServiceErrorCode::RateLimited,
        }
    }
}

impl Shard {
    /// List of nodes that are storing the shard (the leader, and optionally the follower).
    pub fn ingesters(&self) -> impl Iterator<Item = NodeId> + '_ {
        [Some(&self.leader_id), self.follower_id.as_ref()]
            .into_iter()
            .flatten()
            .map(|node_id| NodeId::new(node_id.clone()))
    }
}

impl DocBatchV2 {
    pub fn docs(self) -> impl Iterator<Item = Bytes> {
        let DocBatchV2 {
            doc_buffer,
            doc_lengths,
        } = self;
        doc_lengths
            .into_iter()
            .scan(0, move |start_offset, doc_length| {
                let start = *start_offset;
                let end = start + doc_length as usize;
                *start_offset = end;
                Some(doc_buffer.slice(start..end))
            })
    }

    pub fn is_empty(&self) -> bool {
        self.doc_lengths.is_empty()
    }

    pub fn num_bytes(&self) -> usize {
        self.doc_buffer.len()
    }

    pub fn num_docs(&self) -> usize {
        self.doc_lengths.len()
    }

    #[cfg(any(test, feature = "testsuite"))]
    pub fn for_test(docs: impl IntoIterator<Item = &'static str>) -> Self {
        let mut doc_buffer = Vec::new();
        let mut doc_lengths = Vec::new();

        for doc in docs {
            doc_buffer.extend(doc.as_bytes());
            doc_lengths.push(doc.len() as u32);
        }
        Self {
            doc_lengths,
            doc_buffer: Bytes::from(doc_buffer),
        }
    }
}

impl MRecordBatch {
    pub fn encoded_mrecords(&self) -> impl Iterator<Item = Bytes> + '_ {
        self.mrecord_lengths
            .iter()
            .scan(0, |start_offset, mrecord_length| {
                let start = *start_offset;
                let end = start + *mrecord_length as usize;
                *start_offset = end;
                Some(self.mrecord_buffer.slice(start..end))
            })
    }

    pub fn is_empty(&self) -> bool {
        self.mrecord_lengths.is_empty()
    }

    pub fn num_bytes(&self) -> usize {
        self.mrecord_buffer.len()
    }

    pub fn num_mrecords(&self) -> usize {
        self.mrecord_lengths.len()
    }

    #[cfg(any(test, feature = "testsuite"))]
    pub fn for_test(mrecords: impl IntoIterator<Item = &'static str>) -> Option<Self> {
        let mut mrecord_buffer = Vec::new();
        let mut mrecord_lengths = Vec::new();

        for mrecord in mrecords {
            mrecord_buffer.extend(mrecord.as_bytes());
            mrecord_lengths.push(mrecord.len() as u32);
        }
        Some(Self {
            mrecord_lengths,
            mrecord_buffer: Bytes::from(mrecord_buffer),
        })
    }
}

impl Shard {
    pub fn shard_id(&self) -> &ShardId {
        self.shard_id
            .as_ref()
            .expect("`shard_id` should be a required field")
    }

    pub fn is_open(&self) -> bool {
        self.shard_state().is_open()
    }

    pub fn is_unavailable(&self) -> bool {
        self.shard_state().is_unavailable()
    }

    pub fn is_closed(&self) -> bool {
        self.shard_state().is_closed()
    }

    pub fn queue_id(&self) -> super::types::QueueId {
        queue_id(self.index_uid(), &self.source_id, self.shard_id())
    }

    pub fn publish_position_inclusive(&self) -> &Position {
        self.publish_position_inclusive
            .as_ref()
            .expect("`publish_position_inclusive` should be a required field")
    }
}

impl ShardState {
    pub fn is_open(&self) -> bool {
        *self == ShardState::Open
    }

    pub fn is_unavailable(&self) -> bool {
        *self == ShardState::Unavailable
    }

    pub fn is_closed(&self) -> bool {
        *self == ShardState::Closed
    }

    pub fn as_json_str_name(&self) -> &'static str {
        match self {
            ShardState::Unspecified => "unspecified",
            ShardState::Open => "open",
            ShardState::Unavailable => "unavailable",
            ShardState::Closed => "closed",
        }
    }

    pub fn from_json_str_name(shard_state_json_name: &str) -> Option<Self> {
        match shard_state_json_name {
            "unspecified" => Some(Self::Unspecified),
            "open" => Some(Self::Open),
            "unavailable" => Some(Self::Unavailable),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

impl ShardIds {
    pub fn queue_ids(&self) -> impl Iterator<Item = QueueId> + '_ {
        self.shard_ids
            .iter()
            .map(|shard_id| queue_id(self.index_uid(), &self.source_id, shard_id))
    }
}

impl ShardIdPositions {
    pub fn index_uid(&self) -> &IndexUid {
        self.index_uid
            .as_ref()
            .expect("`index_uid` should be a required field")
    }

    pub fn queue_id_positions(&self) -> impl Iterator<Item = (QueueId, &Position)> + '_ {
        self.shard_positions.iter().map(|shard_position| {
            let queue_id = queue_id(self.index_uid(), &self.source_id, shard_position.shard_id());
            (queue_id, shard_position.publish_position_inclusive())
        })
    }
}

impl ShardIdPosition {
    pub fn shard_id(&self) -> &ShardId {
        self.shard_id
            .as_ref()
            .expect("`shard_id` should be a required field")
    }

    pub fn publish_position_inclusive(&self) -> &Position {
        self.publish_position_inclusive
            .as_ref()
            .expect("`publish_position_inclusive` should be a required field")
    }
}

impl From<PersistFailureReason> for IngestFailureReason {
    fn from(reason: PersistFailureReason) -> Self {
        match reason {
            PersistFailureReason::Unspecified => IngestFailureReason::Unspecified,
            PersistFailureReason::ShardNotFound => IngestFailureReason::NoShardsAvailable,
            PersistFailureReason::ShardClosed => IngestFailureReason::NoShardsAvailable,
            PersistFailureReason::ResourceExhausted => IngestFailureReason::ResourceExhausted,
            PersistFailureReason::RateLimited => IngestFailureReason::RateLimited,
        }
    }
}

impl From<ReplicateFailureReason> for PersistFailureReason {
    fn from(reason: ReplicateFailureReason) -> Self {
        match reason {
            ReplicateFailureReason::Unspecified => PersistFailureReason::Unspecified,
            ReplicateFailureReason::ShardNotFound => PersistFailureReason::ShardNotFound,
            ReplicateFailureReason::ShardClosed => PersistFailureReason::ShardClosed,
            ReplicateFailureReason::ResourceExhausted => PersistFailureReason::ResourceExhausted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shard_state_json_str_name() {
        let shard_state_json_name = ShardState::Unspecified.as_json_str_name();
        let shard_state = ShardState::from_json_str_name(shard_state_json_name).unwrap();
        assert_eq!(shard_state, ShardState::Unspecified);

        let shard_state_json_name = ShardState::Open.as_json_str_name();
        let shard_state = ShardState::from_json_str_name(shard_state_json_name).unwrap();
        assert_eq!(shard_state, ShardState::Open);

        let shard_state_json_name = ShardState::Unavailable.as_json_str_name();
        let shard_state = ShardState::from_json_str_name(shard_state_json_name).unwrap();
        assert_eq!(shard_state, ShardState::Unavailable);

        let shard_state_json_name = ShardState::Closed.as_json_str_name();
        let shard_state = ShardState::from_json_str_name(shard_state_json_name).unwrap();
        assert_eq!(shard_state, ShardState::Closed);

        assert!(ShardState::from_json_str_name("unknown").is_none());
    }

    #[test]
    fn test_ingest_v2_error_grpc_conversion() {
        let ingester_id = NodeId::from("test-ingester");
        let error: IngestV2Error = IngestV2Error::IngesterUnavailable { ingester_id };
        let grpc_status: tonic::Status = error.into();
        let error_serdeser = IngestV2Error::from(grpc_status);
        assert!(
            matches!(error_serdeser, IngestV2Error::IngesterUnavailable { ingester_id } if ingester_id == "test-ingester")
        );
    }
}
