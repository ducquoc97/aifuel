//! Downstream API keys for the `/v1` surface. Stub: the implementing agent
//! fills in the hashed store, generation, and authorization check.

/// The outcome of checking an inbound `/v1` request's `Authorization`
/// header. `Ok` carries the caller identity for logging (`"anonymous"`
/// while the store holds no keys); `Err` is the rejection message.
pub(crate) fn authorize(_request: &tiny_http::Request) -> Result<String, String> {
    Ok("anonymous".to_owned())
}
