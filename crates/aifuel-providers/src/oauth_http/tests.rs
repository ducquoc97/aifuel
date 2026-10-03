use super::*;

fn request() -> RunRequest {
    RunRequest {
        integration: IntegrationId::new("codex:oauth"),
        model: Some("gpt-5".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "hi".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        env: Default::default(),
        interaction_handler: None,
    }
}

#[test]
fn reject_unsupported_gates_every_demand_beyond_prompt_completion() {
    let integration = IntegrationId::new("codex:oauth");
    let denied = |mutate: fn(&mut RunRequest)| {
        let mut req = request();
        mutate(&mut req);
        assert!(
            reject_unsupported(&integration, &req).is_err(),
            "the demand must be rejected before any request is built"
        );
    };
    denied(|req| req.access = AccessMode::WorkspaceWrite);
    denied(|req| req.access = AccessMode::Full);
    denied(|req| req.external_tools = Some(vec!["mcp:x".to_owned()]));
    denied(|req| req.effort = Some("high".to_owned()));
    denied(|req| req.resume = Some("cursor".to_owned()));
    denied(|req| req.account = Some("acc".to_owned()));
    denied(|req| req.output = OutputFormat::Jsonl);
    denied(|req| req.model = None);
    // A read-only prompt completion with a model is accepted.
    assert!(reject_unsupported(&integration, &request()).is_ok());
}

#[test]
fn base64url_decodes_without_padding() {
    assert_eq!(base64url_decode("aGVsbG8"), Some(b"hello".to_vec()));
    assert_eq!(base64url_decode("aGVsbG8="), Some(b"hello".to_vec()));
    assert_eq!(
        base64url_decode("eyJleHAiOjE3MDB9"),
        Some(b"{\"exp\":1700}".to_vec())
    );
    assert_eq!(base64url_decode("not*base64"), None);
}

#[test]
fn jwt_exp_reads_the_claim_from_a_jwt() {
    let payload = base64url_decode("eyJleHAiOjE3MDB9").unwrap();
    assert_eq!(
        jwt_exp(&format!("aaa.{}.sig", "eyJleHAiOjE3MDB9")),
        Some(1700)
    );
    assert_eq!(jwt_exp("not-a-jwt"), None);
    assert_eq!(jwt_exp(&String::from_utf8_lossy(&payload)), None);
}

#[test]
fn mint_session_id_is_uuid_shaped_and_unique() {
    let a = mint_session_id();
    let b = mint_session_id();
    assert_ne!(a, b);
    let parts: Vec<&str> = a.split('-').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(parts[0].len(), 8);
    assert_eq!(parts[1].len(), 4);
    assert_eq!(parts[2].len(), 4);
    assert_eq!(parts[3].len(), 4);
    assert_eq!(parts[4].len(), 12);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
}

#[test]
fn redact_secrets_scrubs_material_and_ignores_empty() {
    let text = "authorization: Bearer abc123 and abc123 again".to_owned();
    assert_eq!(
        redact_secrets(text, &["abc123", ""]),
        "authorization: Bearer <redacted> and <redacted> again"
    );
}

#[test]
fn compiled_evidence_reports_the_in_process_adapter_honestly() {
    let presence = compiled_presence();
    assert_eq!(presence.state, AgentPresenceState::Present);
    assert_eq!(compiled_version().version, None);
    assert_eq!(
        unprobed_authentication().state,
        AgentAuthenticationState::Unknown
    );
}
