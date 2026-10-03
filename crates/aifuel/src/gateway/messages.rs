//! Anthropic-compatible `POST /v1/messages` and
//! `POST /v1/messages/count_tokens`. Stub: the implementing agent fills in
//! the request mapping, response shape, and SSE event stream.

use super::{Gateway, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::StatusCollector;

/// Handle one `/v1/messages*` request end to end, like `chat::completions`.
pub(crate) fn handle<C: StatusCollector>(
    request: tiny_http::Request,
    _gateway: &Gateway,
    _facade: &MonitoringFacade<C>,
    _runtime: &tokio::runtime::Runtime,
) {
    respond_error(
        request,
        501,
        "/v1/messages is not implemented yet",
        "invalid_request_error",
    );
}
