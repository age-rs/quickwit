// Copyright (C) 2021 Quickwit, Inc.
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

use quickwit_metastore::checkpoint::PartitionId;
use rusoto_kinesis::{Kinesis, Record};
use tokio::sync::mpsc;
use tokio::time::Duration;

use crate::source::kinesis::api::{get_records, get_shard_iterator};

#[derive(Debug)]
enum ShardConsumerMessage {
    ShardClosed(String),
    ShardEOF(String),
    Records {
        shard_id: String,
        records: Vec<Record>,
        lag_millis: Option<i64>,
    },
    ShardSplit(Vec<String>),
}

struct ShardConsumer<K: Kinesis> {
    stream_name: String,
    shard_id: String,
    starting_sequence_number: Option<String>,
    eof_enabled: bool,
    kinesis_client: K,
    sink: mpsc::Sender<ShardConsumerMessage>,
}

impl<K: Kinesis + Send + Sync + 'static> ShardConsumer<K> {
    fn new(
        stream_name: String,
        shard_id: String,
        starting_sequence_number: Option<String>,
        eof_enabled: bool,
        kinesis_client: K,
        sink: mpsc::Sender<ShardConsumerMessage>,
    ) -> Self {
        Self {
            stream_name,
            shard_id,
            starting_sequence_number,
            eof_enabled,
            kinesis_client,
            sink,
        }
    }

    fn spawn(self) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        tokio::task::spawn(async move { self.run().await })
    }

    async fn run(&self) -> anyhow::Result<()> {
        let mut shard_iterator_opt = get_shard_iterator(
            &self.kinesis_client,
            &self.stream_name,
            &self.shard_id,
            self.starting_sequence_number.clone(),
        )
        .await?;

        let mut interval = tokio::time::interval(Duration::from_millis(200));

        while let Some(shard_iterator) = shard_iterator_opt {
            interval.tick().await;
            let response = get_records(&self.kinesis_client, shard_iterator).await?;

            if !response.records.is_empty() {
                let message = ShardConsumerMessage::Records {
                    shard_id: self.shard_id.clone(),
                    records: response.records,
                    lag_millis: response.millis_behind_latest,
                };
                self.sink.send(message).await?;
            }
            if let Some(children) = response.child_shards {
                let shard_ids = children.into_iter().map(|child| child.shard_id).collect();
                let message = ShardConsumerMessage::ShardSplit(shard_ids);
                self.sink.send(message).await?;
            }
            if self.eof_enabled && response.millis_behind_latest.unwrap_or(-1) == 0 {
                let message = ShardConsumerMessage::ShardEOF(self.shard_id.clone());
                self.sink.send(message).await?;
                return Ok(());
            };
            shard_iterator_opt = response.next_shard_iterator;
        }
        let message = ShardConsumerMessage::ShardClosed(self.shard_id.clone());
        self.sink.send(message).await?;
        Ok(())
    }
}

#[cfg(all(test, feature = "kinesis-external-service"))]
mod kinesis_localstack_tests {
    use super::*;
    use crate::source::kinesis::api::{list_shards, merge_shards, split_shard};
    use crate::source::kinesis::helpers::tests::{
        get_localstack_client, make_shard_id, parse_shard_id, put_records_into_shards, setup,
        teardown, wait_for_active_stream,
    };

    #[tokio::test]
    async fn test_start_at_horizon() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-start-at-horizon", 1).await?;
        put_records_into_shards(
            &kinesis_client,
            &stream_name,
            [(0, "Record #00"), (0, "Record #01")],
        )
        .await?;
        let shard_id = make_shard_id(0);
        let (tx, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id.clone(),
            None,
            true,
            kinesis_client.clone(),
            tx.clone(),
        );
        let handle = shard_consumer.spawn();
        let message = rx.recv().await.unwrap();
        assert!(
            matches!(message, ShardConsumerMessage::Records { shard_id, records, lag_millis } if records.len() == 2)
        );
        let message = rx.recv().await.unwrap();
        assert!(matches!(
            message,
            ShardConsumerMessage::ShardEOF(eof_shard_id)
        ));
        assert!(handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_start_after_sequence_number() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-start-after-sequence-number", 1).await?;
        let sequence_numbers = put_records_into_shards(
            &kinesis_client,
            &stream_name,
            [(0, "Record #00"), (0, "Record #01")],
        )
        .await?;
        let shard_id = make_shard_id(0);
        let starting_sequence_number = sequence_numbers
            .get(&0)
            .and_then(|sequence_numbers| sequence_numbers.first());
        let (tx, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id.clone(),
            starting_sequence_number.cloned(),
            true,
            kinesis_client.clone(),
            tx.clone(),
        );
        let handle = shard_consumer.spawn();
        let message = rx.recv().await.unwrap();
        assert!(
            matches!(message, ShardConsumerMessage::Records { shard_id, records, lag_millis } if records.len() == 1)
        );
        let message = rx.recv().await.unwrap();
        assert!(matches!(
            message,
            ShardConsumerMessage::ShardEOF(eof_shard_id)
        ));
        assert!(handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_shard_closed() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-shard-closed", 2).await?;
        let shard_id_0 = make_shard_id(0);
        let shard_id_1 = make_shard_id(1);
        let (tx, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id_1.clone(),
            None,
            false,
            kinesis_client.clone(),
            tx.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        merge_shards(&kinesis_client, &stream_name, &shard_id_0, &shard_id_1).await?;
        let shard_consumer_message = rx.recv().await.unwrap();
        assert!(
            matches!(shard_consumer_message, ShardConsumerMessage::ShardClosed(closed_shard_id) if closed_shard_id == shard_id_1)
        );
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_shard_eof() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-shard-eof", 1).await?;
        let shard_id = make_shard_id(0);
        let (tx, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id.clone(),
            None,
            true,
            kinesis_client.clone(),
            tx.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        let shard_consumer_message = rx.recv().await.unwrap();
        assert!(
            matches!(shard_consumer_message, ShardConsumerMessage::ShardEOF(eof_shard_id) if eof_shard_id == shard_id)
        );
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_shard_split() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-shard-split", 1).await?;
        let sequence_numbers = put_records_into_shards(
            &kinesis_client,
            &stream_name,
            [(0, "Record #00"), (0, "Record #01")],
        )
        .await?;
        let shard_id = make_shard_id(0);
        let (tx, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id.clone(),
            None,
            false,
            kinesis_client.clone(),
            tx.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        split_shard(
            &kinesis_client,
            &stream_name,
            &shard_id,
            "1111111111111111111111",
        )
        .await?;
        let shard_consumer_message = rx.recv().await.unwrap();
        assert!(matches!(
            shard_consumer_message,
            ShardConsumerMessage::Records {
                shard_id,
                records,
                lag_millis
            }
        ));
        let shard_consumer_message = rx.recv().await.unwrap();
        assert!(matches!(
            shard_consumer_message,
            ShardConsumerMessage::ShardSplit(shard_ids)
        ));
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }
}
