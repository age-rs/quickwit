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

use elasticsearch_dsl::search::ErrorCause;
use hyper::StatusCode;
use quickwit_index_management::IndexServiceError;
use quickwit_ingest::IngestServiceError;
use quickwit_proto::ingest::IngestV2Error;
use quickwit_proto::ServiceError;
use quickwit_search::SearchError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElasticsearchError {
    #[serde(with = "http_serde::status_code")]
    pub status: StatusCode,
    pub error: ErrorCause,
}

impl ElasticsearchError {
    pub fn new(status: StatusCode, reason_string: String) -> Self {
        ElasticsearchError {
            status,
            error: ErrorCause {
                reason: Some(reason_string),
                caused_by: None,
                root_cause: Vec::new(),
                stack_trace: None,
                suppressed: Vec::new(),
                ty: None,
                additional_details: Default::default(),
            },
        }
    }
}

impl From<SearchError> for ElasticsearchError {
    fn from(search_error: SearchError) -> Self {
        let status = search_error.error_code().to_http_status_code();
        // Fill only reason field to keep it simple.
        let reason = ErrorCause {
            reason: Some(search_error.to_string()),
            caused_by: None,
            root_cause: Vec::new(),
            stack_trace: None,
            suppressed: Vec::new(),
            ty: None,
            additional_details: Default::default(),
        };
        ElasticsearchError {
            status,
            error: reason,
        }
    }
}

impl From<IngestServiceError> for ElasticsearchError {
    fn from(ingest_service_error: IngestServiceError) -> Self {
        let status = ingest_service_error.error_code().to_http_status_code();

        let reason = ErrorCause {
            reason: Some(ingest_service_error.to_string()),
            caused_by: None,
            root_cause: Vec::new(),
            stack_trace: None,
            suppressed: Vec::new(),
            ty: None,
            additional_details: Default::default(),
        };
        ElasticsearchError {
            status,
            error: reason,
        }
    }
}

impl From<IngestV2Error> for ElasticsearchError {
    fn from(ingest_error: IngestV2Error) -> Self {
        let status = ingest_error.error_code().to_http_status_code();

        let reason = ErrorCause {
            reason: Some(ingest_error.to_string()),
            caused_by: None,
            root_cause: Vec::new(),
            stack_trace: None,
            suppressed: Vec::new(),
            ty: None,
            additional_details: Default::default(),
        };
        ElasticsearchError {
            status,
            error: reason,
        }
    }
}

impl From<IndexServiceError> for ElasticsearchError {
    fn from(ingest_error: IndexServiceError) -> Self {
        let status = ingest_error.error_code().to_http_status_code();

        let reason = ErrorCause {
            reason: Some(ingest_error.to_string()),
            caused_by: None,
            root_cause: Vec::new(),
            stack_trace: None,
            suppressed: Vec::new(),
            ty: None,
            additional_details: Default::default(),
        };
        ElasticsearchError {
            status,
            error: reason,
        }
    }
}
