use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// An API error, rendered with upstream's envelope (`src/lib/response.ts`):
/// `{"error": {"message": ..., "code": ..., "status": ...}}`.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("Unauthorized")]
    Unauthorized,
    #[error("Forbidden")]
    Forbidden,
    #[error("Not found")]
    NotFound,
    #[error("Payload too large")]
    PayloadTooLarge,
    #[error("Service unavailable")]
    ServiceUnavailable,
    /// An error with its own code and message, like upstream's
    /// `unauthorized({ code: 'incorrect-username-password' })`.
    #[error("{message}")]
    Coded {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
    /// A response body given verbatim, for upstream responses outside the usual
    /// envelope (such as 2FA lockouts, which add `lockedUntil`).
    #[error("{status}")]
    Custom { status: StatusCode, body: Value },
    /// Logged server-side; the client only sees a generic message.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    /// A 400 with upstream's default message.
    pub fn bad_request() -> Self {
        Self::BadRequest("Bad request".to_owned())
    }

    /// A 401 with a specific code and upstream's default message.
    pub fn unauthorized_code(code: &'static str) -> Self {
        Self::Coded {
            status: StatusCode::UNAUTHORIZED,
            code,
            message: "Unauthorized".to_owned(),
        }
    }

    /// A 401 with a custom message, as upstream's `unauthorized({ message })`.
    pub fn unauthorized_message(message: &str) -> Self {
        Self::coded(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    /// A coded error, as upstream's `badRequest({ code, message })` and friends.
    pub fn coded(status: StatusCode, code: &'static str, message: &str) -> Self {
        Self::Coded {
            status,
            code,
            message: message.to_owned(),
        }
    }

    /// Upstream's `serverError('message')`: a 500 whose message is shown to the client.
    pub fn server_error_message(message: &str) -> Self {
        Self::Coded {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "server-error",
            message: message.to_owned(),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Coded { status, .. } | Self::Custom { status, .. } => *status,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad-request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not-found",
            Self::PayloadTooLarge => "payload-too-large",
            Self::ServiceUnavailable => "service-unavailable",
            Self::Coded { code, .. } => code,
            Self::Custom { .. } => "custom",
            Self::Internal(_) => "server-error",
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl AppError {
    /// The status and JSON body this error renders as. Internal errors are logged
    /// here, since their details are not sent to the client.
    pub fn into_parts(self) -> (StatusCode, Value) {
        if let Self::Custom { status, body } = self {
            return (status, body);
        }

        let status = self.status();
        let message = match &self {
            Self::Internal(error) => {
                tracing::error!(error = ?error, "internal server error");
                "Server error".to_owned()
            }
            other => other.to_string(),
        };

        let body = json!({
            "error": {
                "message": message,
                "code": self.code(),
                "status": status.as_u16(),
            }
        });

        (status, body)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_parts();

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    async fn render(error: AppError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();

        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn renders_upstream_envelope() {
        let cases = [
            (AppError::bad_request(), 400, "Bad request", "bad-request"),
            (AppError::Unauthorized, 401, "Unauthorized", "unauthorized"),
            (AppError::Forbidden, 403, "Forbidden", "forbidden"),
            (AppError::NotFound, 404, "Not found", "not-found"),
            (
                AppError::PayloadTooLarge,
                413,
                "Payload too large",
                "payload-too-large",
            ),
            (
                AppError::ServiceUnavailable,
                503,
                "Service unavailable",
                "service-unavailable",
            ),
        ];

        for (error, status, message, code) in cases {
            let (actual_status, body) = render(error).await;

            assert_eq!(actual_status.as_u16(), status);
            assert_eq!(
                body,
                json!({ "error": { "message": message, "code": code, "status": status } })
            );
        }
    }

    #[tokio::test]
    async fn internal_errors_hide_details() {
        let (status, body) =
            render(AppError::Internal(anyhow::anyhow!("connection refused"))).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            json!({ "error": { "message": "Server error", "code": "server-error", "status": 500 } })
        );
    }
}
