#[allow(dead_code)]
mod support;

#[cfg(unix)]
mod codex_resume {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::support::{TestDirectory, path_with};

    #[test]
    fn codex_resume_uses_exec_resume_with_read_only_sandbox() {
        let directory = TestDirectory::new("codex-resume");
        let executable = directory.path().join("codex");
        fs::write(
            &executable,
            r#"#!/bin/sh
if [ "$1" = "exec" ] && [ "$2" = "--help" ]; then
    printf 'exec --sandbox\n'
    exit 0
fi
[ "$1" = "exec" ] || exit 64
shift
sandbox=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-git-repo-check) shift ;;
        --sandbox)
            [ "$2" = "read-only" ] || exit 64
            sandbox="$2"
            shift 2
            ;;
        resume)
            [ "$sandbox" = "read-only" ] || exit 64
            shift
            [ "$1" = "session-123" ] || exit 64
            shift
            [ "$1" = "continue safely" ] || exit 64
            printf 'accepted resume: sandbox=%s session=%s prompt=%s\n' "$sandbox" "session-123" "$1"
            exit 0
            ;;
        *)
            printf 'unsupported Codex argument: %s\n' "$1" >&2
            exit 64
            ;;
    esac
done
exit 64
"#,
        )
        .expect("strict Codex fixture should be writable");
        let mut permissions = fs::metadata(&executable)
            .expect("Codex fixture should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).expect("Codex fixture should be executable");

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                "codex",
                "--prompt",
                "continue safely",
                "--resume",
                "session-123",
            ])
            .env("PATH", path_with(directory.path()))
            .output()
            .expect("aifuel should start");

        assert!(
            output.status.success(),
            "strict Codex parser rejected the resumed invocation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "accepted resume: sandbox=read-only session=session-123 prompt=continue safely"
        );
    }
}
