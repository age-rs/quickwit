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

#![allow(dead_code)]

use rusoto_kinesis::{Kinesis, Record};
use tokio::sync::mpsc;
use tokio::time::Duration;

use crate::source::kinesis::api::{get_records, get_shard_iterator};

#[derive(Debug)]
enum ShardConsumerMessage {
    NewShards(Vec<String>),
    Records {
        shard_id: String,
        records: Vec<Record>,
        lag_millis: Option<i64>,
    },
    ShardClosed(String),
    ShardEOF(String),
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
                let shard_ids: Vec<String> = children
                    .into_iter()
                    // Suppress duplicate message when two shards are merged.
                    .filter(|child| child.parent_shards.first() == Some(&self.shard_id))
                    .map(|child| child.shard_id)
                    .collect();
                if !shard_ids.is_empty() {
                    let message = ShardConsumerMessage::NewShards(shard_ids);
                    self.sink.send(message).await?;
                }
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
    use crate::source::kinesis::api::{merge_shards, split_shard};
    use crate::source::kinesis::helpers::tests::{
        make_shard_id, put_records_into_shards, setup, teardown,
    };

    async fn recv_message(
        rx: &mut mpsc::Receiver<ShardConsumerMessage>,
    ) -> anyhow::Result<ShardConsumerMessage> {
        match tokio::time::timeout(tokio::time::Duration::from_secs(30), rx.recv()).await {
            Ok(Some(message)) => Ok(message),
            Ok(None) => Err(anyhow::anyhow!("Channel closed.")),
            Err(_) => Err(anyhow::anyhow!("Recv timeout.")),
        }
    }

    #[tokio::test]
    async fn test_shard_eof() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-shard-eof", 1).await?;
        let shard_id_0 = make_shard_id(0);
        let (sink, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id_0.clone(),
            None,
            true,
            kinesis_client.clone(),
            sink.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        let shard_consumer_message = recv_message(&mut rx).await?;
        assert!(matches!(
            shard_consumer_message,
            ShardConsumerMessage::ShardEOF(shard_id) if shard_id == shard_id_0
        ));
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_start_at_horizon() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-start-at-horizon", 1).await?;
        put_records_into_shards(
            &kinesis_client,
            &stream_name,
            [(0, "Record #00"), (0, "Record #01")],
        )
        .await?;
        let shard_id_0 = make_shard_id(0);
        let (sink, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id_0.clone(),
            None,
            true,
            kinesis_client.clone(),
            sink.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::Records { shard_id, records, lag_millis: _ } if shard_id == shard_id_0 && records.len() == 2
            ));
        }
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::ShardEOF(shard_id) if shard_id == shard_id_0
            ));
        }
        assert!(shard_consumer_handle.await.is_ok());
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
        let shard_id_0 = make_shard_id(0);
        let starting_sequence_number = sequence_numbers
            .get(&0)
            .and_then(|sequence_numbers| sequence_numbers.first());
        let (sink, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id_0.clone(),
            starting_sequence_number.cloned(),
            true,
            kinesis_client.clone(),
            sink.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::Records { shard_id, records, lag_millis: _ } if shard_id == shard_id_0 && records.len() == 1
            ));
        }
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::ShardEOF(shard_id) if shard_id == shard_id_0
            ));
        }
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    // This test fails when run against the Localstack Kinesis providers `kinesis-mock` or
    // `kinesalite`.
    #[ignore]
    #[tokio::test]
    async fn test_merge_shards() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-merge-shards", 2).await?;
        let shard_id_0 = make_shard_id(0);
        let shard_id_1 = make_shard_id(1);
        merge_shards(&kinesis_client, &stream_name, &shard_id_0, &shard_id_1).await?;

        let (sink, mut rx) = mpsc::channel(10);
        {
            let shard_consumer = ShardConsumer::new(
                stream_name.clone(),
                shard_id_0.clone(),
                None,
                false,
                kinesis_client.clone(),
                sink.clone(),
            );
            let shard_consumer_handle = shard_consumer.spawn();
            {
                let shard_consumer_message = recv_message(&mut rx).await?;
                assert!(matches!(
                    shard_consumer_message,
                    ShardConsumerMessage::NewShards(shard_ids) if shard_ids == vec![make_shard_id(2)]
                ));
            }
            {
                let shard_consumer_message = recv_message(&mut rx).await?;
                assert!(matches!(
                    shard_consumer_message,
                    ShardConsumerMessage::ShardClosed(shard_id) if shard_id == shard_id_0
                ));
            }
            assert!(shard_consumer_handle.await.is_ok());
        }
        {
            let shard_consumer = ShardConsumer::new(
                stream_name.clone(),
                shard_id_1.clone(),
                None,
                false,
                kinesis_client.clone(),
                sink.clone(),
            );
            let shard_consumer_handle = shard_consumer.spawn();
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::ShardClosed(shard_id) if shard_id == shard_id_1
            ));
            assert!(shard_consumer_handle.await.is_ok());
        }
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }

    // This test fails when run against the Localstack Kinesis providers `kinesis-mock` or
    // `kinesalite`.
    #[ignore]
    #[tokio::test]
    async fn test_split_shard() -> anyhow::Result<()> {
        let (kinesis_client, stream_name) = setup("test-split-shard", 1).await?;
        let shard_id_0 = make_shard_id(0);
        split_shard(&kinesis_client, &stream_name, &shard_id_0, "42").await?;

        let (sink, mut rx) = mpsc::channel(10);
        let shard_consumer = ShardConsumer::new(
            stream_name.clone(),
            shard_id_0.clone(),
            None,
            false,
            kinesis_client.clone(),
            sink.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn();
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::NewShards(shard_ids) if shard_ids == vec![make_shard_id(1), make_shard_id(2)]
            ));
        }
        {
            let shard_consumer_message = recv_message(&mut rx).await?;
            assert!(matches!(
                shard_consumer_message,
                ShardConsumerMessage::ShardClosed(shard_id) if shard_id == shard_id_0
            ));
        }
        assert!(shard_consumer_handle.await.is_ok());
        teardown(&kinesis_client, &stream_name).await;
        Ok(())
    }
}
