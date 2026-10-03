//! OpenAI Responses API: `POST /v1/responses` and
//! `POST /v1/responses/compact`. Stub: the implementing agent fills in the
//! request mapping, response shape, and SSE event stream.

use super::{Gateway, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::StatusCollector;

/// Handle one `/v1/responses*` request end to end, like
/// `chat::completions`.
pub(crate) fn handle<C: StatusCollector>(
    request: tiny_http::Request,
    _gateway: &Gateway,
    _facade: &MonitoringFacade<C>,
    _runtime: &tokio::runtime::Runtime,
) {
    respond_error(
        request,
        501,
        "/v1/responses is not implemented yet",
        "invalid_request_error",
    );
}
