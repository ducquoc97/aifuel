//! Checkpoints: the hidden git refs the runtime records for
//! workspace-mutating Agent Runs.
//!
//! A Checkpoint is a commit holding the worktree's full state - tracked
//! modifications and untracked-but-not-ignored files - stored under
//! `refs/aifuel/checkpoints/`. The plumbing never touches the user's HEAD,
//! index, or branch: the working tree is staged through a throwaway
//! `GIT_INDEX_FILE`, committed with `commit-tree`, and anchored with
//! `update-ref`. Checkpoint commits chain per Agent Session, so a run's
//! `diffstat` measures the tree against the session's previous checkpoint
//! (or `HEAD` for the first), not against an ever-growing baseline.
//!
//! `checkpoint.restore` is the only mutation path: `read-tree --reset -u`
//! rewrites index and worktree to the checkpoint tree without moving HEAD,
//! and `clean -fd` drops files the checkpoint never captured. It discards
//! working state by contract, so callers validate the checkpoint belongs
//! to the session before invoking it.

use crate::dispatch::{CommandOutcome, store_error};
use crate::pump;
use crate::runtime::AgentRuntime;
use aifuel_core::{
    AgentEventKind, CheckpointId, CommandId, Receipt, ReceiptCode, RunId, SessionId, SessionStatus,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The refs namespace Checkpoints live under. Hidden from branch and tag
/// listings; each ref anchors its commit against garbage collection and
/// names it for `checkpoint.restore`.
const REF_PREFIX: &str = "refs/aifuel/checkpoints/";

/// The author and committer stamped on checkpoint commits: the runtime
/// records the fact, so the identity names the runtime rather than
/// inheriting (or requiring) the repo's user configuration.
const AUTHOR_ENV: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "aifuel"),
    ("GIT_AUTHOR_EMAIL", "aifuel@localhost"),
    ("GIT_COMMITTER_NAME", "aifuel"),
    ("GIT_COMMITTER_EMAIL", "aifuel@localhost"),
];

/// A recorded Checkpoint: the commit id doubles as the [`CheckpointId`]
/// since the ref namespace names each commit directly.
pub(crate) struct RecordedCheckpoint {
    pub checkpoint_id: CheckpointId,
    /// The `git diff --stat` text measured against the checkpoint's base.
    pub diffstat: String,
}

/// Why `checkpoint.restore` could not complete.
pub(crate) enum RestoreError {
    /// No checkpoint ref with this id exists in the worktree; the commit
    /// was removed or the workspace moved.
    UnknownCheckpoint,
    /// Git plumbing failed; the message is diagnostic.
    Failed(String),
}

/// The git worktree root containing `cwd`, or `None` when `cwd` is not
/// inside a worktree (no repository, a bare repository, a `.git`
/// directory, or no git binary). Callers treat `None` as "no checkpoint
/// surface", never as an error.
pub(crate) fn worktree_root(cwd: &Path) -> Option<PathBuf> {
    let output = git(
        cwd,
        &["rev-parse", "--is-inside-work-tree", "--show-toplevel"],
    )
    .ok()?;
    let mut lines = output.lines();
    let inside = lines.next()?;
    let top = lines.next()?;
    (inside == "true").then(|| PathBuf::from(top))
}

/// Capture the worktree's full state as one checkpoint commit and anchor
/// it under [`REF_PREFIX`].
///
/// `base` is the session's previous checkpoint, so the diffstat reports
/// this run's own changes; the first checkpoint diffs against `HEAD`, and
/// a repository without commits diffs against the empty tree. Returns
/// `Ok(None)` when the worktree matches the base - a run that produced no
/// diff records no Checkpoint, matching the contract's `read_only` rule.
pub(crate) fn create(
    root: &Path,
    run_id: &RunId,
    base: Option<&CheckpointId>,
) -> Result<Option<RecordedCheckpoint>, String> {
    // The base commit is the previous checkpoint or HEAD; a ref that
    // vanished between runs fails here rather than silently changing
    // baseline.
    let base_commit = match base {
        Some(checkpoint) => Some(checkpoint.as_str().to_owned()),
        None => git(root, &["rev-parse", "--verify", "HEAD"]).ok(),
    };
    let base_tree = match &base_commit {
        Some(commit) => git(root, &["rev-parse", &format!("{commit}^{{tree}}")])?,
        None => empty_tree(root)?,
    };
    // Stage the whole worktree through a throwaway index so the user's
    // real index and HEAD are never read-touched or written.
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?);
    let index = git_dir.join(format!(
        "aifuel-checkpoint-{}-{}.index",
        std::process::id(),
        next_index_id()
    ));
    let staged = (|| {
        git_index(root, &index, &["read-tree", &base_tree])?;
        git_index(root, &index, &["add", "--all"])?;
        git_index(root, &index, &["write-tree"])
    })();
    let _ = std::fs::remove_file(&index);
    let tree = staged?;
    let diffstat = git(root, &["diff", "--stat", &base_tree, &tree])?;
    if diffstat.is_empty() {
        return Ok(None);
    }
    let message = format!("aifuel checkpoint {}", run_id.as_str());
    let mut args = vec!["commit-tree", tree.as_str()];
    if let Some(parent) = &base_commit {
        args.extend(["-p", parent.as_str()]);
    }
    args.extend(["-m", message.as_str()]);
    let mut commit = git_command(root);
    for (key, value) in AUTHOR_ENV {
        commit.env(key, value);
    }
    let commit = finish(&mut commit, &args)?;
    git(
        root,
        &["update-ref", &format!("{REF_PREFIX}{commit}"), &commit],
    )?;
    Ok(Some(RecordedCheckpoint {
        checkpoint_id: CheckpointId::new(commit),
        diffstat,
    }))
}

/// Materialize one recorded Checkpoint back into the worktree: the index
/// and working tree take the checkpoint's tree while HEAD and the branch
/// stay untouched, then untracked files the checkpoint never captured are
/// removed. Ignored files are kept (`clean` without `-x`).
pub(crate) fn restore(root: &Path, checkpoint_id: &CheckpointId) -> Result<(), RestoreError> {
    let oid = checkpoint_id.as_str();
    // Resolving through the ref namespace proves the commit is one this
    // runtime recorded; a bare oid could name any commit in the repo.
    let resolved = git(
        root,
        &[
            "rev-parse",
            "--verify",
            &format!("{REF_PREFIX}{oid}^{{commit}}"),
        ],
    )
    .map_err(|_| RestoreError::UnknownCheckpoint)?;
    if resolved != oid {
        return Err(RestoreError::Failed(
            "the checkpoint ref no longer names the recorded commit".to_owned(),
        ));
    }
    git(root, &["read-tree", "--reset", "-u", oid]).map_err(RestoreError::Failed)?;
    git(root, &["clean", "-fd"]).map_err(RestoreError::Failed)?;
    Ok(())
}

/// The empty tree oid for this repository, written by `git mktree` on an
/// empty stdin (`output` closes stdin). Computing it keeps the unborn-HEAD
/// base valid for sha256 repositories too.
fn empty_tree(root: &Path) -> Result<String, String> {
    git(root, &["mktree"])
}

/// A unique throwaway index path per checkpoint, so concurrent sessions
/// sharing one repository never share an index file.
fn next_index_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Run one git command in `dir` with only the terminal prompt pinned off.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    finish(&mut git_command(dir), args)
}

/// Run one git command against a throwaway index file.
fn git_index(dir: &Path, index: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = git_command(dir);
    command.env("GIT_INDEX_FILE", index.as_os_str());
    finish(&mut command, args)
}

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        // A credential or pager prompt could otherwise park the pump
        // thread forever.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    command
}

fn finish(command: &mut Command, args: &[&str]) -> Result<String, String> {
    let output = command
        .args(args)
        .output()
        .map_err(|error| format!("git {} could not run: {error}", args[0]))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git {} failed: {}", args.join(" "), stderr.trim()))
    }
}

impl AgentRuntime {
    /// `checkpoint.restore`: materialize one recorded Checkpoint back into
    /// the session's workspace. The command discards working state by
    /// contract, so it runs only on an idle-or-interrupted session against
    /// a Checkpoint that session's log recorded, and the fact lands as
    /// `checkpoint.restored` like any other. The workspace itself is the
    /// session's persisted `cwd`; a live adapter session is not required
    /// because the runtime owns the refs.
    pub(crate) fn checkpoint_restore(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    ) -> CommandOutcome {
        let session = match self.store.agent_session(&session_id) {
            Ok(Some(session)) => session,
            Ok(None) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::UnknownSession,
                    "no Agent Session with that id is known",
                );
            }
            Err(error) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        };
        match session.status {
            SessionStatus::Closed => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::InvalidState,
                    "the Agent Session is closed",
                );
            }
            SessionStatus::Working | SessionStatus::WaitingApproval | SessionStatus::Compacting => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::InvalidState,
                    "an Agent Run is in flight; the workspace cannot reset mid-run",
                );
            }
            SessionStatus::Idle | SessionStatus::Interrupted => {}
        }
        let recorded = match self.store.session_checkpoints(&session_id) {
            Ok(recorded) => recorded,
            Err(error) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        };
        if !recorded
            .iter()
            .any(|checkpoint| checkpoint.checkpoint_id == checkpoint_id)
        {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::InvalidState,
                "no Checkpoint with that id is recorded for this session",
            );
        }
        let Some(root) = worktree_root(&session.cwd) else {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::InvalidState,
                "the session's workspace is not a git worktree",
            );
        };
        if let Err(error) = restore(&root, &checkpoint_id) {
            return match error {
                RestoreError::UnknownCheckpoint => CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::InvalidState,
                    "the Checkpoint ref is not present in the session's workspace",
                ),
                RestoreError::Failed(message) => CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    format!("the Checkpoint could not be restored: {message}"),
                ),
            };
        }
        match self.store.append(
            &session_id,
            AgentEventKind::CheckpointRestored { checkpoint_id },
        ) {
            Ok(event) => {
                let inner = self.inner.lock().expect("runtime mutex");
                pump::broadcast(&inner, &session_id, &event);
                CommandOutcome::ok(Receipt::ok(command_id, event.seq, Some(session_id), None))
            }
            Err(error) => CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            ),
        }
    }
}
