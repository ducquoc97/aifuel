//! Dashboard admin endpoints for the gateway: `GET /api/gateway/keys`,
//! `POST /api/gateway/keys`, `POST /api/gateway/keys/revoke`,
//! `GET /api/gateway/logs`, `GET /api/gateway/providers`. Stub: the
//! implementing agent fills in handlers over `keys`, `logs`, and the
//! executable adapter set.

use super::{Gateway, respond_error};

/// Handle one `/api/gateway/*` request. Runs under the dashboard's strict
/// same-origin guard, not the relaxed `/v1` guard.
pub(crate) fn handle(request: tiny_http::Request, _gateway: &Gateway) {
    respond_error(
        request,
        501,
        "the gateway admin api is not implemented yet",
        "invalid_request_error",
    );
}
