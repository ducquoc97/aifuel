//! Legacy OpenAI text `POST /v1/completions`. Stub: maps `prompt` onto the
//! chat pipeline as one user message.

use super::{Gateway, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::StatusCollector;

/// Handle one `/v1/completions` request end to end.
pub(crate) fn handle<C: StatusCollector>(
    request: tiny_http::Request,
    _gateway: &Gateway,
    _facade: &MonitoringFacade<C>,
    _runtime: &tokio::runtime::Runtime,
) {
    respond_error(
        request,
        501,
        "/v1/completions is not implemented yet",
        "invalid_request_error",
    );
}
