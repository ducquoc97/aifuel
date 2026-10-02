//! `POST /v1/chat/completions`: resolve the inbound `model` selector to one
//! or more execution targets, run the first that accepts the request, and
//! answer in OpenAI Chat Completions shape - SSE `chat.completion.chunk`
//! frames for `stream: true`, one `chat.completion` JSON otherwise.
//!
//! Failover only happens before the response commits: an attempt that ends
//! without emitting a delta lets the next ranked candidate answer the same
//! HTTP request. Once the first chunk is written the attempt owns the
//! stream and a mid-run failure surfaces as an error frame, matching how
//! `run --provider auto` stops the chain once a run reached its provider.

use super::flatten::flatten_messages;
use super::types::ChatRequest;
use super::{Gateway, cors_headers, read_body, respond, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, AgentRunOutputHandler, IntegrationId,
    OutputFormat, RunCancellationToken, RunRequest, RunResult, RunStatus, StatusCollector,
    StatusReport, TokenUsage,
};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Heartbeat interval on an open SSE stream, so a dead client is detected
/// without waiting for the next token.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// One ranked execution target: the integration to attempt and the model
/// override the inbound selector pinned, if any.
struct Attempt {
    integration: IntegrationId,
    model: Option<String>,
}

/// One attempt's terminal routing decision: `Done` means the HTTP request
/// was answered (successfully or with a terminal error); `Retry` records
/// why and lets the next candidate take the uncommitted request.
enum Flow {
    Done,
    Retry(String),
}

/// The two messages a run worker can post back: one answer delta, or the
/// finished run outcome.
enum Event {
    Delta(String),
    Done(Result<RunResult, AgentRunError>),
}

/// Normalized answer deltas from the adapter into the attempt channel.
#[derive(Debug)]
struct DeltaSink(mpsc::Sender<Event>);

impl AgentRunOutputHandler for DeltaSink {
    fn on_output(&self, delta: &str) {
        let _ = self.0.send(Event::Delta(delta.to_owned()));
    }
}

/// Handle one `POST /v1/chat/completions` request end to end.
pub(crate) fn completions<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => {
            respond_error(request, 400, message, "invalid_request_error");
            return;
        }
    };
    let chat: ChatRequest = match serde_json::from_slice(&body) {
        Ok(chat) => chat,
        Err(error) => {
            respond_error(
                request,
                400,
                &format!("invalid chat completion request: {error}"),
                "invalid_request_error",
            );
            return;
        }
    };
    let model = chat.model.as_deref().unwrap_or_default().trim();
    if model.is_empty() {
        respond_error(request, 400, "model is required", "invalid_request_error");
        return;
    }
    if chat.messages.is_empty() {
        respond_error(
            request,
            400,
            "messages must not be empty",
            "invalid_request_error",
        );
        return;
    }
    let prompt = flatten_messages(&chat.messages);
    if prompt.trim().is_empty() {
        respond_error(
            request,
            400,
            "messages contain no usable text; media-only content is not supported",
            "invalid_request_error",
        );
        return;
    }
    let attempts = match attempts(gateway, model, &|| Gateway::status(facade, runtime)) {
        Ok(attempts) => attempts,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    run_chain(request, gateway, &chat, model, &prompt, attempts);
}

/// Resolve the inbound `model` string to the attempt chain - see the
/// `crate::gateway` module docs for the addressing convention.
fn attempts(
    gateway: &Gateway,
    model: &str,
    status: &dyn Fn() -> StatusReport,
) -> Result<Vec<Attempt>, (u16, String)> {
    if model == aifuel_core::AUTO_PROVIDER {
        return plan(model, None, status);
    }
    if let Some(filter) = model.strip_prefix("auto/") {
        return plan(model, Some(filter.to_owned()), status);
    }
    if let Some((selector, pinned)) = model.split_once('/') {
        return match gateway.resolve(selector) {
            Ok(integration) => Ok(vec![Attempt {
                integration,
                model: Some(pinned.to_owned()),
            }]),
            Err(AgentRunError::AmbiguousIntegration { provider, .. }) => Err((
                400,
                format!(
                    "{provider} maps to multiple integrations; name an integration id explicitly"
                ),
            )),
            Err(_) => Err((
                404,
                format!("unknown integration or provider {selector:?} in model {model:?}"),
            )),
        };
    }
    match gateway.resolve(model) {
        Ok(integration) => Ok(vec![Attempt {
            integration,
            model: None,
        }]),
        Err(AgentRunError::AmbiguousIntegration { provider, .. }) => Err((
            400,
            format!("{provider} maps to multiple integrations; name an integration id explicitly"),
        )),
        // Not a selector at all: treat the string as a catalog model id and
        // let the planner pick the ranked provider advertising it.
        Err(_) => plan(model, Some(model.to_owned()), status),
    }
}

/// Plan the `auto` chain over a fresh cached status snapshot.
fn plan(
    requested: &str,
    model: Option<String>,
    status: &dyn Fn() -> StatusReport,
) -> Result<Vec<Attempt>, (u16, String)> {
    let report = status();
    let registry = crate::integration_registry().map_err(|error| (500, error.clone()))?;
    let config_dir = crate::aifuel_config_dir().map_err(|error| (500, error.clone()))?;
    let credentials = aifuel_providers::CredentialStore::new(config_dir);
    let candidates = crate::route_planner::provider_candidates(
        model.as_deref(),
        &report,
        &registry,
        &credentials,
    )
    .map_err(|error| (500, error))?;
    if candidates.is_empty() {
        return Err((
            404,
            match &model {
                Some(model) => format!(
                    "{requested:?} found no Discovered Provider or keyed integration advertising {model:?}; refresh evidence with `aifuel model refresh`"
                ),
                None => format!(
                    "{requested:?} found no Discovered Provider or keyed API-key integration; `aifuel status` shows which credential sources are present"
                ),
            },
        ));
    }
    Ok(candidates
        .into_iter()
        .map(|candidate| Attempt {
            integration: candidate.integration,
            model: model.clone(),
        })
        .collect())
}

/// Walk the attempt chain, streaming or buffering per `chat.stream`.
fn run_chain(
    request: tiny_http::Request,
    gateway: &Gateway,
    chat: &ChatRequest,
    model_echo: &str,
    prompt: &str,
    attempts: Vec<Attempt>,
) {
    let completion_id = format!(
        "chatcmpl-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut slot = Some(request);
    let mut writer: Option<Box<dyn Write + Send>> = None;
    let mut last_failure = "no executable provider candidate".to_owned();
    let mut attempted = false;
    for attempt in attempts {
        let Some(adapter) = gateway.adapter(&attempt.integration) else {
            last_failure = format!("{} has no execution adapter", attempt.integration);
            continue;
        };
        let run_request = RunRequest {
            integration: attempt.integration.clone(),
            model: attempt.model.clone(),
            effort: None,
            external_tools: None,
            account: None,
            prompt: prompt.to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: AccessMode::ReadOnly,
            resume: None,
            timeout: None,
            env: Default::default(),
            interaction_handler: None,
        };
        if let Err(error) = adapter.validate(&run_request) {
            last_failure = error.to_string();
            continue;
        }
        if attempted {
            eprintln!(
                "aifuel: gateway failing over to {} ({last_failure})",
                attempt.integration
            );
        } else {
            eprintln!("aifuel: gateway selected {}", attempt.integration);
        }
        attempted = true;
        match drive(
            adapter,
            run_request,
            &mut slot,
            &mut writer,
            chat.stream,
            chat.stream_options
                .as_ref()
                .and_then(|options| options.include_usage)
                .unwrap_or(false),
            &completion_id,
            created,
            model_echo,
        ) {
            Flow::Done => return,
            Flow::Retry(reason) => last_failure = reason,
        }
    }
    // Every candidate declined or failed before committing: the request is
    // still answerable with a normal error response.
    if let Some(request) = slot.take() {
        let status = if attempted { 502 } else { 404 };
        respond_error(request, status, &last_failure, "server_error");
    }
}

/// Run one attempt, forwarding answer deltas onto the SSE stream once it
/// commits. [`Flow::Done`] always means the request has been fully
/// answered - including "the client went away", which cancels the run.
#[allow(clippy::too_many_arguments)]
fn drive(
    adapter: Arc<dyn AgentExecutionAdapter>,
    run_request: RunRequest,
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    stream: bool,
    include_usage: bool,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Flow {
    let (sender, receiver) = mpsc::channel::<Event>();
    let cancellation = RunCancellationToken::new();
    let worker_cancel = cancellation.clone();
    let sink = DeltaSink(sender.clone());
    let worker = thread::spawn(move || {
        let result = adapter.execute_with_output_handler(&run_request, &worker_cancel, &sink);
        let _ = sender.send(Event::Done(result));
    });

    // `collected` reconstructs the answer for adapters that report their
    // text only through deltas; `saw_delta` keeps a streamed attempt from
    // re-emitting the same text from `RunResult.output`.
    let mut collected = String::new();
    let mut saw_delta = false;
    let flow = loop {
        match receiver.recv_timeout(KEEPALIVE) {
            Ok(Event::Delta(delta)) => {
                saw_delta = true;
                collected.push_str(&delta);
                if stream
                    && let Err(()) =
                        write_delta(slot, writer, completion_id, created, model_echo, &delta)
                {
                    cancellation.cancel();
                    break Flow::Done;
                }
            }
            Ok(Event::Done(result)) => {
                break finish(
                    result,
                    slot,
                    writer,
                    stream,
                    include_usage,
                    completion_id,
                    created,
                    model_echo,
                    &collected,
                    saw_delta,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(writer) = writer.as_mut()
                    && writer
                        .write_all(b": keep-alive\n\n")
                        .and_then(|()| writer.flush())
                        .is_err()
                {
                    cancellation.cancel();
                    break Flow::Done;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Flow::Retry("the provider run ended without a result".to_owned());
            }
        }
    };
    let _ = worker.join();
    flow
}

/// Classify one finished attempt into the routing decision.
#[allow(clippy::too_many_arguments)]
fn finish(
    result: Result<RunResult, AgentRunError>,
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    stream: bool,
    include_usage: bool,
    completion_id: &str,
    created: u64,
    model_echo: &str,
    collected: &str,
    saw_delta: bool,
) -> Flow {
    match result {
        Ok(result) if result.status == RunStatus::Succeeded => {
            let answer = if saw_delta {
                collected
            } else {
                result.output.as_str()
            };
            if let Some(writer) = writer.as_mut() {
                // Deltas already streamed: close out the committed response.
                let _ = write_terminal(
                    writer,
                    result.usage.as_ref(),
                    include_usage,
                    completion_id,
                    created,
                    model_echo,
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else if stream {
                // The answer arrived whole (a non-streaming adapter): commit
                // now and replay it as one chunk sequence.
                if commit(slot, writer, completion_id, created, model_echo).is_err() {
                    return Flow::Done;
                }
                let writer = writer.as_mut().expect("commit stores the writer");
                if !answer.is_empty() {
                    let _ = write_all(
                        writer,
                        chunk_frame(
                            completion_id,
                            created,
                            model_echo,
                            json!({"content": answer}),
                            None,
                        )
                        .as_bytes(),
                    );
                }
                let _ = write_terminal(
                    writer,
                    result.usage.as_ref(),
                    include_usage,
                    completion_id,
                    created,
                    model_echo,
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                respond(
                    request,
                    200,
                    serde_json::to_vec(&completion_body(
                        completion_id,
                        created,
                        model_echo,
                        answer,
                        result.usage.as_ref(),
                    ))
                    .expect("a completion body serializes"),
                    Some("application/json"),
                    cors_headers(),
                );
            }
            Flow::Done
        }
        Ok(result) => {
            let reason = result
                .error
                .clone()
                .or(result.diagnostics.clone())
                .unwrap_or_else(|| format!("the provider run ended as {}", result.status.as_str()));
            if writer.is_none() && retriable_result(&result) {
                Flow::Retry(reason)
            } else if writer.is_some() {
                let _ = write_error_frame(writer.as_mut().expect("writer present"), &reason);
                Flow::Done
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                respond_error(request, status_for_result(&result), &reason, "server_error");
                Flow::Done
            }
        }
        Err(error) => {
            if writer.is_none() && retriable_error(&error) {
                Flow::Retry(error.to_string())
            } else if writer.is_some() {
                let _ =
                    write_error_frame(writer.as_mut().expect("writer present"), &error.to_string());
                Flow::Done
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                respond_error(
                    request,
                    status_for_error(&error),
                    &error.to_string(),
                    "server_error",
                );
                Flow::Done
            }
        }
    }
}

/// Whether a completed-but-failed run may move to the next candidate: only
/// provider-reported quota or rate-limit exhaustion, mirroring the `auto`
/// chain's post-execution retry rule.
fn retriable_result(result: &RunResult) -> bool {
    result.quota_exhausted
        || result
            .error
            .as_deref()
            .is_some_and(crate::route_planner::provider_quota_wording)
        || result
            .diagnostics
            .as_deref()
            .is_some_and(crate::route_planner::provider_quota_wording)
}

/// Whether a launch-time error may move to the next candidate: everything
/// except timeouts and cancellation, mirroring `auto`'s error verdicts.
fn retriable_error(error: &AgentRunError) -> bool {
    !matches!(error, AgentRunError::Timeout(_) | AgentRunError::Cancelled)
}

fn status_for_result(result: &RunResult) -> u16 {
    if result.timed_out || result.status == RunStatus::Timeout {
        504
    } else {
        502
    }
}

fn status_for_error(error: &AgentRunError) -> u16 {
    match error {
        AgentRunError::Timeout(_) => 504,
        AgentRunError::UnsupportedIntegration(_) | AgentRunError::AmbiguousIntegration { .. } => {
            404
        }
        AgentRunError::InvalidRequest(_) => 400,
        _ => 502,
    }
}

/// Write the SSE head and the assistant role chunk - the point of no
/// return after which the attempt owns the stream.
fn commit(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Result<(), ()> {
    let request = slot.take().ok_or(())?;
    let mut w = request.into_writer();
    write_all(&mut w, SSE_HEAD.as_bytes())?;
    let role = chunk_frame(
        completion_id,
        created,
        model_echo,
        json!({"role": "assistant", "content": ""}),
        None,
    );
    write_all(&mut w, role.as_bytes())?;
    *writer = Some(w);
    Ok(())
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\n";

/// Commit on the first delta when streaming, then write the delta chunk.
fn write_delta(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    completion_id: &str,
    created: u64,
    model_echo: &str,
    delta: &str,
) -> Result<(), ()> {
    if writer.is_none() {
        commit(slot, writer, completion_id, created, model_echo)?;
    }
    let frame = chunk_frame(
        completion_id,
        created,
        model_echo,
        json!({"content": delta}),
        None,
    );
    write_all(
        writer.as_mut().expect("commit stores the writer"),
        frame.as_bytes(),
    )
}

/// The terminal chunk (`finish_reason: "stop"`) plus the spec-shaped usage
/// chunk when the caller asked for it.
fn write_terminal(
    writer: &mut Box<dyn Write + Send>,
    usage: Option<&TokenUsage>,
    include_usage: bool,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Result<(), ()> {
    let stop = chunk_frame(completion_id, created, model_echo, json!({}), Some("stop"));
    write_all(writer, stop.as_bytes())?;
    if include_usage {
        let usage_frame = usage_chunk(completion_id, created, model_echo, usage);
        write_all(writer, usage_frame.as_bytes())?;
    }
    Ok(())
}

/// An SSE error frame for a failure after the stream committed, then the
/// `[DONE]` sentinel clients expect.
fn write_error_frame(writer: &mut Box<dyn Write + Send>, message: &str) -> Result<(), ()> {
    let frame = format!(
        "data: {}\n\n",
        json!({"error": {"message": message, "type": "server_error", "param": null, "code": null}})
    );
    write_all(writer, frame.as_bytes())?;
    write_all(writer, b"data: [DONE]\n\n")
}

fn write_all(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> Result<(), ()> {
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|_| ())
}

/// One `chat.completion.chunk` SSE frame.
fn chunk_frame(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    delta: Value,
    finish_reason: Option<&str>,
) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_echo,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    )
}

/// The final `{"choices": [], "usage": ...}` chunk shape
/// `stream_options.include_usage` callers wait on. `usage` is null when the
/// provider reported none - missing counts stay unknown, never zero.
fn usage_chunk(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    usage: Option<&TokenUsage>,
) -> String {
    let usage = usage.map(|usage| {
        json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        })
    });
    format!(
        "data: {}\n\n",
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_echo,
            "choices": [],
            "usage": usage,
        })
    )
}

/// The non-streamed `chat.completion` body.
fn completion_body(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    answer: &str,
    usage: Option<&TokenUsage>,
) -> Value {
    json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": model_echo,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": answer},
            "finish_reason": "stop",
        }],
        "usage": usage.map(|usage| json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SSE frames must match the shape OpenAI SDKs and litellm parse:
    /// `data:` prefix, `chat.completion.chunk` object, `choices[].delta`,
    /// and a `choices: []` usage chunk before `[DONE]` - Cline historically
    /// crashed on usage chunks carrying malformed choices, and several
    /// clients key usage accounting off this exact shape.
    #[test]
    fn chunk_frames_are_openai_shaped() {
        let delta = chunk_frame("chatcmpl-1", 10, "auto", json!({"content": "hi"}), None);
        let frame: Value = serde_json::from_str(delta.trim_start_matches("data: ").trim())
            .expect("a chunk frame is json");
        assert_eq!(frame["object"], "chat.completion.chunk");
        assert_eq!(frame["choices"][0]["delta"]["content"], "hi");
        assert!(delta.ends_with("\n\n"));

        let usage = usage_chunk(
            "chatcmpl-1",
            10,
            "auto",
            Some(&TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
            }),
        );
        let frame: Value = serde_json::from_str(usage.trim_start_matches("data: ").trim())
            .expect("a usage frame is json");
        assert_eq!(frame["choices"], json!([]));
        assert_eq!(frame["usage"]["total_tokens"], 7);
    }

    #[test]
    fn usage_chunk_reports_unknown_as_null() {
        // A provider that reported nothing must not fabricate zeroes.
        let frame = usage_chunk("chatcmpl-1", 10, "auto", None);
        let parsed: Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(parsed["usage"], Value::Null);
    }

    #[test]
    fn completion_body_is_openai_shaped() {
        let body = completion_body("chatcmpl-1", 10, "auto", "done", None);
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "done");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"], Value::Null);
    }

    #[test]
    fn retry_verdicts_mirror_the_auto_chain() {
        // Only quota exhaustion retries after a run was accepted; only
        // timeout/cancellation stop the chain before execution.
        let mut failed = RunResult {
            run_id: "r".to_owned(),
            local_session_id: "s".to_owned(),
            session_id: None,
            resumed_from: None,
            provider_id: aifuel_core::ProviderId::new("codex"),
            integration_id: IntegrationId::new("codex"),
            requested_model: None,
            requested_effort: None,
            effective_model: None,
            effective_effort: None,
            requested_account_id: None,
            account_id: None,
            execution_mode: aifuel_core::ExecutionMode::PromptOnly,
            permission_profile: AccessMode::ReadOnly,
            status: RunStatus::Failed,
            exit_code: Some(1),
            output: String::new(),
            error: Some("HTTP 429: rate limit".to_owned()),
            diagnostics: None,
            usage: None,
            timed_out: false,
            quota_exhausted: true,
            working_directory: std::path::PathBuf::new(),
        };
        assert!(retriable_result(&failed));
        failed.quota_exhausted = false;
        failed.error = Some("exit code 1".to_owned());
        assert!(!retriable_result(&failed));

        assert!(retriable_error(&AgentRunError::InvalidRequest(
            "launch failed".to_owned()
        )));
        assert!(!retriable_error(&AgentRunError::Timeout("t".to_owned())));
        assert!(!retriable_error(&AgentRunError::Cancelled));
    }
}
