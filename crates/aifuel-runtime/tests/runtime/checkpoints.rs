//! Checkpoints and `quota.observed`: the post-run facts the runtime owns.
//!
//! Checkpoints are hidden git refs under `refs/aifuel/checkpoints/`
//! recorded by the runtime - not the adapter - for workspace-mutating
//! runs. These tests drive real temporary git repositories so the ref
//! plumbing, the diffstat chaining, and `checkpoint.restore`'s workspace
//! rewind are verified end to end.

use crate::support::{
    ExecScript, FakeAdapter, FakeScript, cli_runtime, collect_run, collect_until, consumer, create,
    create_with_access, created_session, fake_runtime, next_id, receipt_code, receipt_snapshot,
    run_start, subscribe, test_dir,
};
use aifuel_core::{
    AccessMode, AgentCommand, AgentEvent, AgentEventKind, CheckpointId, QuotaSummary, ReceiptCode,
    RunId,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a git command against the test repository, failing the test on a
/// non-zero exit.
fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

/// A git worktree under `dir` with one committed file.
fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir creates");
    git(&repo, &["init", "--quiet"]);
    std::fs::write(repo.join("tracked.txt"), "v1\n").expect("file writes");
    git(&repo, &["add", "tracked.txt"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
    );
    repo
}

/// A runtime over the fake adapter serving sessions in `dir` workspaces.
fn scripted_runtime(dir: &Path) -> (aifuel_app::RunStore, aifuel_runtime::AgentRuntime) {
    fake_runtime(
        dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![
            FakeScript::Complete {
                deltas: vec![],
                cursor: None,
            },
            FakeScript::Complete {
                deltas: vec![],
                cursor: None,
            },
        ],
    )
}

/// The first `checkpoint.created` fact in a collected event page.
fn checkpoint_created(events: &[AgentEvent]) -> (RunId, CheckpointId, String) {
    events
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::CheckpointCreated {
                run_id,
                checkpoint_id,
                diffstat,
            } => Some((run_id.clone(), checkpoint_id.clone(), diffstat.clone())),
            _ => None,
        })
        .expect("a checkpoint.created fact")
}

/// A `checkpoint.restore` command for `checkpoint_id`.
fn restore(session_id: &aifuel_core::SessionId, checkpoint_id: &CheckpointId) -> AgentCommand {
    AgentCommand::CheckpointRestore {
        command_id: next_id(),
        session_id: session_id.clone(),
        checkpoint_id: checkpoint_id.clone(),
    }
}

/// A completed workspace-write run records a hidden ref capturing the
/// whole working tree - tracked modifications and untracked files - and
/// emits `checkpoint.created` after `run.completed`.
#[test]
fn write_run_records_a_checkpoint_ref() {
    let dir = test_dir("checkpoint-create");
    let repo = init_repo(&dir);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    // The state present when the run completes is the run's workspace.
    std::fs::write(repo.join("draft.txt"), "draft\n").expect("file writes");
    std::fs::write(repo.join("tracked.txt"), "v2\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "write"), &consumer);

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (run_id, checkpoint_id, diffstat) = checkpoint_created(&collected);
    // The fact follows its run's terminal fact in causal order.
    let completed = collected
        .iter()
        .position(|event| matches!(event.kind, AgentEventKind::RunCompleted { .. }))
        .expect("run.completed precedes the checkpoint");
    assert!(
        collected
            .iter()
            .position(|event| matches!(event.kind, AgentEventKind::CheckpointCreated { .. }))
            .unwrap()
            > completed
    );
    assert!(
        collected
            .iter()
            .any(|event| matches!(&event.kind, AgentEventKind::RunStarted { run_id: id, .. } if *id == run_id)),
        "the checkpoint names the run it captures"
    );
    assert!(
        diffstat.contains("draft.txt") && diffstat.contains("tracked.txt"),
        "the diffstat names the captured changes: {diffstat}"
    );
    // The hidden ref anchors the commit: the checkpoint id is the commit.
    assert_eq!(
        git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/aifuel/checkpoints/{checkpoint_id}"),
            ],
        ),
        checkpoint_id.as_str()
    );
    // The checkpoint commit captured the untracked file too.
    let listed = git(
        &repo,
        &["show", "--name-only", "--format=", checkpoint_id.as_str()],
    );
    assert!(listed.lines().any(|name| name == "draft.txt"));
    // The user's HEAD and index were never touched: the worktree still
    // shows the run's changes as uncommitted.
    assert!(
        !git(&repo, &["status", "--porcelain"]).is_empty(),
        "the worktree state is untouched"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `read_only` run records no Checkpoint and leaves the refs namespace
/// empty - it produces no diff by contract.
#[test]
fn read_only_run_records_no_checkpoint() {
    let dir = test_dir("checkpoint-readonly");
    let repo = init_repo(&dir);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(
        create_with_access(&repo, "fake-a", AccessMode::ReadOnly),
        &consumer,
    ));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(repo.join("draft.txt"), "draft\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "read"), &consumer);

    let collected = collect_run(&events);
    assert!(
        collected
            .iter()
            .all(|event| !matches!(event.kind, AgentEventKind::CheckpointCreated { .. })),
        "a read-only run never records a Checkpoint"
    );
    assert!(
        git(&repo, &["for-each-ref", "refs/aifuel/checkpoints"]).is_empty(),
        "no checkpoint ref exists"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `checkpoint.restore` rewinds the worktree to the recorded state:
/// modified files return, checkpoint-captured files return, files created
/// after the checkpoint are dropped - and the fact lands in the log.
#[test]
fn checkpoint_restore_rewinds_the_worktree() {
    let dir = test_dir("checkpoint-restore");
    let repo = init_repo(&dir);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(repo.join("tracked.txt"), "v2\n").expect("file writes");
    std::fs::write(repo.join("added.txt"), "added\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "write"), &consumer);
    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (_, checkpoint_id, _) = checkpoint_created(&collected);

    // Post-checkpoint working state the restore discards.
    let head_before = git(&repo, &["rev-parse", "--verify", "HEAD"]);
    std::fs::write(repo.join("tracked.txt"), "v3\n").expect("file writes");
    std::fs::remove_file(repo.join("added.txt")).expect("file removes");
    std::fs::write(repo.join("later.txt"), "later\n").expect("file writes");

    let outcome = runtime.dispatch(restore(&session_id, &checkpoint_id), &consumer);
    assert!(
        outcome.receipt.ok,
        "restore succeeds: {:?}",
        outcome.receipt.outcome
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).expect("file reads"),
        "v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("added.txt")).expect("file reads"),
        "added\n"
    );
    assert!(
        !repo.join("later.txt").exists(),
        "files the checkpoint never captured are dropped"
    );
    // The fact lands as `checkpoint.restored` on the session's channel.
    let restored = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointRestored { .. })
    });
    assert!(restored.iter().any(|event| matches!(
        &event.kind,
        AgentEventKind::CheckpointRestored { checkpoint_id: id } if *id == checkpoint_id
    )));
    // HEAD never moved: the checkpoint is worktree state, not a commit.
    assert_eq!(
        git(&repo, &["rev-parse", "--verify", "HEAD"]),
        head_before,
        "restore rewinds the worktree without touching the branch"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Checkpoints chain per session, so each diffstat reports its own run's
/// changes; the snapshot lists them in record order.
#[test]
fn runs_chain_checkpoints_and_the_snapshot_lists_them() {
    let dir = test_dir("checkpoint-chain");
    let repo = init_repo(&dir);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);

    std::fs::write(repo.join("a.txt"), "a\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "first"), &consumer);
    let first = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (run_one, checkpoint_one, _) = checkpoint_created(&first);

    std::fs::write(repo.join("b.txt"), "b\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "second"), &consumer);
    let second = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (run_two, checkpoint_two, diffstat_two) = checkpoint_created(&second);

    assert_ne!(run_one, run_two);
    // The second checkpoint's parent is the first, so its diffstat covers
    // the second run only, not the accumulated workspace.
    assert_eq!(
        git(
            &repo,
            &["rev-parse", "--verify", &format!("{checkpoint_two}^")]
        ),
        checkpoint_one.as_str()
    );
    assert!(diffstat_two.contains("b.txt"));
    assert!(
        !diffstat_two.contains("a.txt"),
        "the per-run diffstat does not repeat the first run: {diffstat_two}"
    );

    let outcome = runtime.dispatch(subscribe(&session_id, 0), &consumer);
    let snapshot = receipt_snapshot(&outcome.receipt);
    let ids: Vec<&CheckpointId> = snapshot
        .checkpoints
        .iter()
        .map(|checkpoint| &checkpoint.checkpoint_id)
        .collect();
    assert_eq!(ids, [&checkpoint_one, &checkpoint_two]);
    assert_eq!(snapshot.checkpoints[0].run_id, run_one);
    assert_eq!(snapshot.checkpoints[1].run_id, run_two);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A repository without commits still checkpoints: the diff base is the
/// empty tree and the commit has no parent.
#[test]
fn unborn_head_repo_still_checkpoints() {
    let dir = test_dir("checkpoint-unborn");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir creates");
    git(&repo, &["init", "--quiet"]);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(repo.join("main.rs"), "fn main() {}\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "write"), &consumer);

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (_, checkpoint_id, diffstat) = checkpoint_created(&collected);
    assert!(diffstat.contains("main.rs"), "diffstat: {diffstat}");
    // The root checkpoint commit has no parent.
    assert_eq!(
        git(&repo, &["rev-list", "--count", checkpoint_id.as_str()]),
        "1"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A workspace that is not a git worktree takes no Checkpoint and reports
/// no error - the run simply completes.
#[test]
fn non_git_worktree_records_no_checkpoint() {
    let dir = test_dir("checkpoint-nongit");
    let workdir = dir.join("plain");
    std::fs::create_dir_all(&workdir).expect("workdir creates");
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&workdir, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(workdir.join("out.txt"), "out\n").expect("file writes");
    let started = runtime.dispatch(run_start(&session_id, "write"), &consumer);
    assert!(started.receipt.ok);

    let collected = collect_run(&events);
    assert!(
        collected
            .iter()
            .all(|event| !matches!(event.kind, AgentEventKind::CheckpointCreated { .. }))
    );
    assert!(
        collected
            .iter()
            .all(|event| !matches!(event.kind, AgentEventKind::Error { .. })),
        "a non-git workspace is quiet, not an error"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `checkpoint.restore` rejects a checkpoint the session never recorded
/// and an unknown session, as receipts rather than workspace damage.
#[test]
fn restore_rejects_checkpoints_the_session_did_not_record() {
    let dir = test_dir("checkpoint-reject");
    let repo = init_repo(&dir);
    let (_store, runtime) = scripted_runtime(&dir);
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(repo.join("draft.txt"), "draft\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "write"), &consumer);
    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::CheckpointCreated { .. })
    });
    let (_, checkpoint_id, _) = checkpoint_created(&collected);

    // A checkpoint id the session never recorded - even one shaped like a
    // real commit - is rejected, not resolved.
    let bogus = CheckpointId::new("0".repeat(40));
    let outcome = runtime.dispatch(restore(&session_id, &bogus), &consumer);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    let outcome = runtime.dispatch(
        restore(
            &aifuel_core::SessionId::new("no-such-session"),
            &checkpoint_id,
        ),
        &consumer,
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::UnknownSession);
    // The worktree kept its post-run state: the rejections touched nothing.
    assert_eq!(
        std::fs::read_to_string(repo.join("draft.txt")).expect("file reads"),
        "draft\n"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `quota.observed` lands after `run.completed` when the integration's
/// Monitoring Collection Contract reports an observation, carrying the
/// integration id and the observed summary verbatim.
#[test]
fn quota_observed_is_emitted_post_run() {
    let dir = test_dir("quota-observed");
    let repo = init_repo(&dir);
    let quota = QuotaSummary {
        remaining_pct: Some(42.5),
        resets_at: Some(1_700_000_000.0),
        depleted: false,
    };
    let adapter = FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
    )
    .with_scripts(vec![FakeScript::Complete {
        deltas: vec![],
        cursor: None,
    }])
    .with_quota(quota);
    let (_store, runtime) = crate::support::runtime_at(
        &dir,
        vec![std::sync::Arc::new(adapter)],
        vec![crate::support::fake_descriptor_with_monitoring()],
    );
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "fake-a"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    runtime.dispatch(run_start(&session_id, "work"), &consumer);

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::QuotaObserved { .. })
    });
    let observed = collected
        .iter()
        .position(|event| matches!(event.kind, AgentEventKind::QuotaObserved { .. }))
        .unwrap();
    let completed = collected
        .iter()
        .position(|event| matches!(event.kind, AgentEventKind::RunCompleted { .. }))
        .expect("run.completed precedes the observation");
    assert!(observed > completed);
    match &collected[observed].kind {
        AgentEventKind::QuotaObserved {
            integration_id,
            quota: reported,
        } => {
            assert_eq!(integration_id.as_str(), crate::support::FAKE_INTEGRATION);
            assert_eq!(*reported, quota);
        }
        other => panic!("expected quota.observed: {other:?}"),
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a Monitoring Collection Contract - the real `CliAdapter`'s
/// case - a run emits no `quota.observed`, and the runtime still records
/// the Checkpoint itself because Checkpoints are runtime-owned, not
/// adapter-declared.
#[test]
fn no_quota_event_without_a_monitoring_contract() {
    let dir = test_dir("quota-absent");
    let repo = init_repo(&dir);
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Succeed {
            deltas: vec![],
            session_id: None,
        }],
    );
    let consumer = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&repo, "scripted-model"), &consumer));
    let events = runtime.events(&consumer).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer);
    std::fs::write(repo.join("draft.txt"), "draft\n").expect("file writes");
    runtime.dispatch(run_start(&session_id, "work"), &consumer);

    let collected = collect_run(&events);
    assert!(
        collected
            .iter()
            .all(|event| !matches!(event.kind, AgentEventKind::QuotaObserved { .. })),
        "no contract, no observation - never a fabricated summary"
    );
    // The CliAdapter declares `checkpoints: false`, yet the run still
    // checkpoints: the runtime owns the ref, the flag describes only what
    // the adapter could do.
    assert!(
        collected
            .iter()
            .any(|event| matches!(event.kind, AgentEventKind::CheckpointCreated { .. })),
        "the runtime records the Checkpoint even when the adapter cannot"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
