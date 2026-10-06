//! `/v1/audio/speech` and `/v1/audio/transcriptions`: the OpenAI audio
//! contract proxied to an HTTP-capable Provider Integration, mirroring
//! how `/v1/embeddings` relays the sibling non-chat endpoint.
//!
//! Speech takes a JSON body and answers audio bytes; transcriptions
//! arrives as `multipart/form-data` with the audio file inline. Both
//! resolve `model` through the shared `forward` machinery - the selector
//! convention, aliases, combos, and `auto` ranking all apply - then POST
//! `{base_url}/audio/<path>` directly. For transcriptions the resolved
//! model rewrites the `model` form field in place: the gateway carries
//! the multipart envelope untouched rather than re-encoding a form it
//! can already forward.
//!
//! Only `WireApi::OpenAiChat` endpoints whose provider documents an audio
//! surface are eligible - `openai` today; a custom `providers.json`
//! integration under provider id `openai` inherits the same paths, which
//! is how local compatible servers join.

use super::{Gateway, forward, read_body, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{ExecutionConfig, StatusCollector, WireApi};
use aifuel_providers::CredentialStore;
use serde_json::Value;

/// `POST /v1/audio/speech`: text in, audio bytes out.
const SPEECH: forward::Surface = forward::Surface {
    name: "audio",
    path: |descriptor| audio_path(descriptor, "audio/speech"),
    // `tts-1` is the documented default-tier speech model.
    default_model: |provider| (provider == "openai").then_some("tts-1"),
};

/// `POST /v1/audio/transcriptions`: a multipart form in, a transcript
/// JSON out.
const TRANSCRIPTIONS: forward::Surface = forward::Surface {
    name: "audio",
    path: |descriptor| audio_path(descriptor, "audio/transcriptions"),
    // `whisper-1` is the long-standing transcription model id.
    default_model: |provider| (provider == "openai").then_some("whisper-1"),
};

/// The audio paths exist only on the OpenAI-compatible surface of
/// providers that document them.
fn audio_path(
    descriptor: &aifuel_providers::IntegrationDescriptor,
    path: &'static str,
) -> Option<&'static str> {
    let ExecutionConfig::Http {
        protocol: WireApi::OpenAiChat,
        ..
    } = &descriptor.integration.execution
    else {
        return None;
    };
    (descriptor.provider().as_str() == "openai").then_some(path)
}

/// Handle one `/v1/audio/speech` request: JSON in, upstream audio out.
pub(crate) fn speech<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let (inbound, model) = match json_selector(&mut request) {
        Ok(pair) => pair,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    let (candidates, credentials) = match resolve(&request, &SPEECH, &model) {
        Ok(pair) => pair,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    let _ = (gateway, facade);
    forward::serve(
        request,
        &SPEECH,
        &model,
        &candidates,
        &credentials,
        &|candidate, auth| {
            let body = forward::upstream_body(&inbound, candidate.model.as_deref());
            let bytes = serde_json::to_vec(&body)
                .map_err(|error| format!("the request body could not be serialized: {error}"))?;
            // Speech answers audio, not JSON: `Accept` stays open so the
            // upstream's own audio content type governs the response.
            runtime.block_on(aifuel_providers::post_raw(
                &candidate.endpoint,
                auth,
                candidate.path,
                bytes,
                "application/json",
                "*/*",
            ))
        },
    );
}

/// Handle one `/v1/audio/transcriptions` request: a multipart form in,
/// upstream JSON out. The inbound `model` form field is the selector;
/// the forwarded body rewrites it to the resolved provider-native id.
pub(crate) fn transcriptions<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_default();
    let Some(boundary) = multipart_boundary(&content_type) else {
        respond_error(
            request,
            400,
            "transcriptions requires a multipart/form-data body",
            "invalid_request_error",
        );
        return;
    };
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => {
            respond_error(request, 400, message, "invalid_request_error");
            return;
        }
    };
    let model = match multipart_field(&body, &boundary, "model")
        .and_then(|(start, end)| std::str::from_utf8(&body[start..end]).ok())
    {
        Some(model) if !model.trim().is_empty() => model.trim().to_owned(),
        _ => {
            respond_error(request, 400, "model is required", "invalid_request_error");
            return;
        }
    };
    let (candidates, credentials) = match resolve(&request, &TRANSCRIPTIONS, &model) {
        Ok(pair) => pair,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    let _ = (gateway, facade);
    forward::serve(
        request,
        &TRANSCRIPTIONS,
        &model,
        &candidates,
        &credentials,
        &|candidate, auth| {
            // The resolved model replaces the selector inside the form -
            // the same rewrite the JSON surfaces apply to `model`, done
            // at the byte level so the file part travels untouched.
            let forwarded = match candidate.model.as_deref() {
                Some(resolved) => match multipart_field(&body, &boundary, "model") {
                    Some((start, end)) => {
                        let mut rewritten = Vec::with_capacity(body.len() + resolved.len());
                        rewritten.extend_from_slice(&body[..start]);
                        rewritten.extend_from_slice(resolved.as_bytes());
                        rewritten.extend_from_slice(&body[end..]);
                        rewritten
                    }
                    // A part vanished between the outer read and here -
                    // send the form as it arrived and let the endpoint
                    // answer for itself.
                    None => body.clone(),
                },
                // No pin resolved: strip nothing and let the endpoint's
                // own contract answer rather than guessing.
                None => body.clone(),
            };
            runtime.block_on(aifuel_providers::post_raw(
                &candidate.endpoint,
                auth,
                candidate.path,
                forwarded,
                &content_type,
                "application/json",
            ))
        },
    );
}

/// Parse the request as a JSON object and pull the `model` selector -
/// the shared prelude for the JSON-shaped audio surface.
fn json_selector(request: &mut tiny_http::Request) -> Result<(Value, String), (u16, String)> {
    let body = read_body(request).map_err(|message| (400, message.to_owned()))?;
    let inbound: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            return Err((400, "the request must be a JSON object".to_owned()));
        }
        Err(error) => return Err((400, format!("invalid request: {error}"))),
    };
    let model = inbound["model"].as_str().unwrap_or_default().trim().to_owned();
    if model.is_empty() {
        return Err((400, "model is required".to_owned()));
    }
    Ok((inbound, model))
}

/// The shared tail: key permits, registry, credentials, evidence, and the
/// ranked candidate chain for `model` under the given audio `surface`.
fn resolve(
    request: &tiny_http::Request,
    surface: &'static forward::Surface,
    model: &str,
) -> Result<(Vec<forward::Candidate>, CredentialStore), (u16, String)> {
    super::keys::require_permits(request, model).map_err(|reason| (403, reason))?;
    let registry = crate::integration_registry().map_err(|error| (500, error))?;
    let config_dir = crate::aifuel_config_dir().map_err(|error| (500, error))?;
    let credentials = CredentialStore::new(config_dir);
    let discovery = aifuel_providers::DiscoveryContext::from_environment().ok();
    let evidence = discovery
        .as_ref()
        .map(|discovery| registry.evidence_context(discovery, &credentials));
    let candidates = forward::resolve_chain(
        surface,
        model,
        &registry,
        &credentials,
        evidence.as_ref(),
        0,
    )?;
    Ok((candidates, credentials))
}

/// The `boundary` parameter of a `multipart/form-data` content type,
/// quoted or bare.
fn multipart_boundary(content_type: &str) -> Option<String> {
    let mime = content_type.split(';').next()?.trim();
    if !mime.eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    for part in content_type.split(';').skip(1) {
        let (name, value) = part.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Locate the byte range of the `name` field's value inside a multipart
/// `body` delimited by `boundary`: the part headers end at the first
/// blank line and the value ends before the next boundary marker.
fn multipart_field(body: &[u8], boundary: &str, name: &str) -> Option<(usize, usize)> {
    let marker = format!("--{boundary}");
    let needle = format!("name=\"{name}\"");
    let mut cursor = 0;
    while cursor < body.len() {
        // Each part opens at a boundary line; the headers run to the
        // blank line, the value to the boundary that follows it.
        let start = find_subslice(&body[cursor..], marker.as_bytes())? + cursor;
        let headers_start = start + marker.len();
        let headers_end = find_subslice(&body[headers_start..], b"\r\n\r\n")? + headers_start;
        let value_start = headers_end + 4;
        let value_end = find_subslice(&body[value_start..], b"\r\n--")? + value_start;
        let headers = std::str::from_utf8(&body[headers_start..headers_end]).ok()?;
        if headers.contains(&needle) {
            return Some((value_start, value_end));
        }
        cursor = value_end + 2;
    }
    None
}

/// The first index of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary parameter must come out of `multipart/form-data`
    /// content types in quoted or bare form; any other media type is not
    /// a form.
    #[test]
    fn multipart_boundary_parses_quoted_and_bare() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=----x7Y"),
            Some("----x7Y".to_owned())
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"abc def\""),
            Some("abc def".to_owned())
        );
        assert_eq!(multipart_boundary("application/json"), None);
        assert_eq!(
            multipart_boundary("multipart/form-data"),
            None
        );
    }

    /// The `model` field's value range must be located exactly - the
    /// rewrite replaces those bytes and nothing else, so a file part's
    /// content can never shift.
    #[test]
    fn multipart_field_finds_the_named_part_value() {
        let boundary = "----b";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nBINARY\x00DATA\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nopenai/audio\r\n--{boundary}--\r\n"
        )
        .into_bytes();
        let (start, end) = multipart_field(&body, boundary, "model")
            .expect("the model part exists");
        assert_eq!(&body[start..end], b"openai/audio");
        // A name prefix must not match a different field.
        let shadowed = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model_extra\"\r\n\r\nno\r\n--{boundary}--\r\n"
        )
        .into_bytes();
        assert!(multipart_field(&shadowed, boundary, "model").is_none());
    }
}
