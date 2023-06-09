use super::headers::client::HeadersRequest;
use crate::{consensus, db};
use reth_network_api::ReputationChangeKind;
use reth_primitives::{BlockHashOrNumber, BlockNumber, Header, WithPeerId, H256};
use std::ops::RangeInclusive;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// Result alias for result of a request.
pub type RequestResult<T> = Result<T, RequestError>;

/// Result with [PeerId][reth_primitives::PeerId]
pub type PeerRequestResult<T> = RequestResult<WithPeerId<T>>;

/// Helper trait used to validate responses.
pub trait EthResponseValidator {
    /// Determine whether the response matches what we requested in [HeadersRequest]
    fn is_likely_bad_headers_response(&self, request: &HeadersRequest) -> bool;

    /// Return the response reputation impact if any
    fn reputation_change_err(&self) -> Option<ReputationChangeKind>;
}

impl EthResponseValidator for RequestResult<Vec<Header>> {
    fn is_likely_bad_headers_response(&self, request: &HeadersRequest) -> bool {
        match self {
            Ok(headers) => {
                let request_length = headers.len() as u64;

                if request_length <= 1 && request.limit != request_length {
                    return true
                }

                match request.start {
                    BlockHashOrNumber::Number(block_number) => headers
                        .first()
                        .map(|header| block_number != header.number)
                        .unwrap_or_default(),
                    BlockHashOrNumber::Hash(_) => {
                        // we don't want to hash the header
                        false
                    }
                }
            }
            Err(_) => true,
        }
    }

    /// [RequestError::ChannelClosed] is not possible here since these errors are mapped to
    /// `ConnectionDropped`, which will be handled when the dropped connection is cleaned up.
    ///
    /// [RequestError::ConnectionDropped] should be ignored here because this is already handled
    /// when the dropped connection is handled.
    ///
    /// [RequestError::UnsupportedCapability] is not used yet because we only support active session
    /// for eth protocol.
    fn reputation_change_err(&self) -> Option<ReputationChangeKind> {
        if let Err(err) = self {
            match err {
                RequestError::ChannelClosed => None,
                RequestError::ConnectionDropped => None,
                RequestError::UnsupportedCapability => None,
                RequestError::Timeout => Some(ReputationChangeKind::Timeout),
                RequestError::BadResponse => None,
            }
        } else {
            None
        }
    }
}

/// Error variants that can happen when sending requests to a session.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum RequestError {
    #[error("Closed channel to the peer.")]
    ChannelClosed,
    #[error("Connection to a peer dropped while handling the request.")]
    ConnectionDropped,
    #[error("Capability Message is not supported by remote peer.")]
    UnsupportedCapability,
    #[error("Request timed out while awaiting response.")]
    Timeout,
    #[error("Received bad response.")]
    BadResponse,
}

// === impl RequestError ===

impl RequestError {
    /// Indicates whether this error is retryable or fatal.
    pub fn is_retryable(&self) -> bool {
        matches!(self, RequestError::Timeout | RequestError::ConnectionDropped)
    }

    /// Whether the error happened because the channel was closed.
    pub fn is_channel_closed(&self) -> bool {
        matches!(self, RequestError::ChannelClosed)
    }
}

impl<T> From<mpsc::error::SendError<T>> for RequestError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        RequestError::ChannelClosed
    }
}

impl From<oneshot::error::RecvError> for RequestError {
    fn from(_: oneshot::error::RecvError) -> Self {
        RequestError::ChannelClosed
    }
}

/// The download result type
pub type DownloadResult<T> = Result<T, DownloadError>;

/// The downloader error type
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /* ==================== HEADER ERRORS ==================== */
    /// Header validation failed
    #[error("Failed to validate header {hash}. Details: {error}.")]
    HeaderValidation {
        /// Hash of header failing validation
        hash: H256,
        /// The details of validation failure
        #[source]
        error: consensus::ConsensusError,
    },
    /// Received an invalid tip
    #[error("Received invalid tip: {received:?}. Expected {expected:?}.")]
    InvalidTip {
        /// The hash of the received tip
        received: H256,
        /// The hash of the expected tip
        expected: H256,
    },
    /// Received a tip with an invalid tip number
    #[error("Received invalid tip number: {received:?}. Expected {expected:?}.")]
    InvalidTipNumber {
        /// The block number of the received tip
        received: u64,
        /// The block number of the expected tip
        expected: u64,
    },
    /// Received a response to a request with unexpected start block
    #[error("Headers response starts at unexpected block: {received:?}. Expected {expected:?}.")]
    HeadersResponseStartBlockMismatch {
        /// The block number of the received tip
        received: u64,
        /// The hash of the expected tip
        expected: u64,
    },
    /// Received headers with less than expected items.
    #[error("Received less headers than expected: {received:?}. Expected {expected:?}.")]
    HeadersResponseTooShort {
        /// How many headers we received.
        received: u64,
        /// How many headers we expected.
        expected: u64,
    },
    /* ==================== BODIES ERRORS ==================== */
    /// Block validation failed
    #[error("Failed to validate body for header {hash}. Details: {error}.")]
    BodyValidation {
        /// Hash of header failing validation
        hash: H256,
        /// The details of validation failure
        #[source]
        error: consensus::ConsensusError,
    },
    /// Received more bodies than requested.
    #[error("Received more bodies than requested. Expected: {expected}. Received: {received}")]
    TooManyBodies {
        /// How many bodies we received.
        received: usize,
        /// How many bodies we expected.
        expected: usize,
    },
    /// Headers missing from the database.
    #[error("Header missing from the database: {block_number}")]
    MissingHeader {
        /// Missing header block number.
        block_number: BlockNumber,
    },
    /// Body range invalid
    #[error("Requested body range is invalid: {range:?}.")]
    InvalidBodyRange {
        /// Invalid block number range.
        range: RangeInclusive<BlockNumber>,
    },
    /* ==================== COMMON ERRORS ==================== */
    /// Timed out while waiting for request id response.
    #[error("Timed out while waiting for response.")]
    Timeout,
    /// Received empty response while expecting non empty
    #[error("Received empty response.")]
    EmptyResponse,
    /// Error while executing the request.
    #[error(transparent)]
    RequestError(#[from] RequestError),
    /// Error while reading data from database.
    #[error(transparent)]
    DatabaseError(#[from] db::DatabaseError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_likely_bad_headers_response() {
        let request =
            HeadersRequest { start: 0u64.into(), limit: 0, direction: Default::default() };
        let headers: Vec<Header> = vec![];
        assert!(!Ok(headers).is_likely_bad_headers_response(&request));

        let request =
            HeadersRequest { start: 0u64.into(), limit: 1, direction: Default::default() };
        let headers: Vec<Header> = vec![];
        assert!(Ok(headers).is_likely_bad_headers_response(&request));
    }
}
