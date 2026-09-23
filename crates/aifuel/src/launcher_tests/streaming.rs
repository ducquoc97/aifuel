use super::*;

#[test]
fn managed_text_run_streams_deltas_without_repeating_final_output() {
    let manager = aifuel_app::RunManager::new(vec![Arc::new(StreamingAdapter)]);
    let mut request = request();
    request.output = OutputFormat::Text;
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let result = execute_with_manager(
        &request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        false,
    )
    .expect("streaming run should finish");

    assert_eq!(result.output.as_deref(), Some("first answer"));
    assert_eq!(output, b"first answer");
}

#[test]
fn managed_jsonl_run_emits_ordered_structured_run_events() {
    let manager = aifuel_app::RunManager::new(vec![Arc::new(StreamingAdapter)]);
    let mut request = request();
    request.output = OutputFormat::Jsonl;
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    execute_with_manager(
        &request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        false,
    )
    .expect("JSONL run should finish");

    let records: Vec<serde_json::Value> = String::from_utf8(output)
        .expect("event output should be UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each event should be JSON"))
        .collect();
    assert!(records.len() >= 5);
    assert!(records.iter().all(|record| record["type"] == "run_event"));
    let sequences = records
        .iter()
        .map(|record| {
            record["event"]["sequence"]
                .as_u64()
                .expect("event sequence should be numeric")
        })
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(records[2]["event"]["data"], "first ");
    assert_eq!(records[3]["event"]["data"], "answer");
}
