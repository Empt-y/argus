//! One error shape for every endpoint.
//!
//! Clients get a JSON body they can parse and a status code they can act on.
//! Internal errors are logged with their full chain and reported to the client
//! as a bare string: an operator reading the journal needs the SQL, and a phone
//! on the LAN does not.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Store(#[from] argus_store::StoreError),
    #[error(transparent)]
    Tile(#[from] argus_tiles::TileError),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: String,
}

impl ApiError {
    fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::Store(_) => "internal",
            Self::Tile(argus_tiles::TileError::OutOfRange { .. }) => "bad_request",
            Self::Tile(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Tile(argus_tiles::TileError::OutOfRange { .. }) => StatusCode::BAD_REQUEST,
            Self::Store(_) | Self::Tile(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // The distinction matters: a client mistake is the client's business
        // and is returned verbatim, while a server fault is ours and the detail
        // stays in the journal where it can name a table or a connection string
        // without shipping either to the network.
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("request failed: {self}");
            let mut source = std::error::Error::source(&self);
            while let Some(cause) = source {
                tracing::error!("  caused by: {cause}");
                source = cause.source();
            }
            "internal error; see the server log".to_string()
        } else {
            self.to_string()
        };

        (
            status,
            axum::Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code(),
                    message,
                },
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
