//! Execute a tool under the section 6.3 run contract.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::frame;
use crate::protocol::tool::{RunInput, RunOutput, codes};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{}: tool exceeded {timeout_s}s", codes::TIMEOUT)]
    Timeout { timeout_s: u64 },
    #[error("{}: {}: {source}", codes::NOT_EXECUTABLE, bin.display())]
    NotExecutable { bin: PathBuf, source: io::Error },
    #[error("{code}: tool process I/O: {0}", code = frame::codes::INTERNAL)]
    Io(#[from] io::Error),
}

/// Start one tool process. Its deadline covers stdin, stdout, stderr and exit.
pub async fn run(bin: &Path, input: RunInput) -> Result<RunOutput, RunError> {
    let not_executable = |source| RunError::NotExecutable { bin: bin.to_owned(), source };
    // Resolve the configured file before setting the child's cwd.
    let bin = std::path::absolute(bin).map_err(not_executable)?;
    let arguments = serde_json::to_vec(&input.arguments).expect("JSON arguments");
    let mut child = Command::new(&bin)
        .current_dir(input.cwd)
        .envs(input.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(not_executable)?;
    let mut group = ProcessGroup(Some(child.id().expect("new child has a pid") as libc::pid_t));
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let write = async move {
        match stdin.write_all(&arguments).await {
            // An executable can exit or close its input without consuming all of it.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            result => result,
        }
    };
    let collect = async {
        let ((), stdout, stderr) = tokio::try_join!(write, read_output(stdout), read_output(stderr))?;
        // Keep the leader unreaped while its descendants might still hold the pipes.
        let status = child.wait().await?;
        Ok::<_, io::Error>(RunOutput {
            exit_code: status.code().unwrap_or_else(|| 128 + status.signal().expect("Unix exit signal")),
            stdout,
            stderr,
        })
    };

    let result = tokio::time::timeout(Duration::from_secs(input.timeout_s), collect).await;
    let error = match result {
        Ok(Ok(output)) => {
            group.0 = None;
            return Ok(output);
        }
        Ok(Err(e)) => RunError::Io(e),
        Err(_) => RunError::Timeout { timeout_s: input.timeout_s },
    };
    group.kill();
    if let Err(e) = child.kill().await {
        tracing::warn!(tool = %bin.display(), "could not kill and reap tool: {e}");
    }
    Err(error)
}

async fn read_output(mut pipe: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Kill descendants on a deadline, I/O error or cancellation while the leader's
/// PID still identifies this group. Tokio reaps the direct child.
struct ProcessGroup(Option<libc::pid_t>);

impl ProcessGroup {
    fn kill(&mut self) {
        if let Some(pgid) = self.0.take() {
            // SAFETY: a negative ID targets the private group created at spawn.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;
    use ulid::Ulid;

    use super::*;
    use crate::protocol::tool::ENV_TURN;

    fn input(cwd: &Path) -> RunInput {
        RunInput { arguments: json!({ "value": "two words" }), cwd: cwd.into(), env: Default::default(), timeout_s: 5 }
    }

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("tool.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[tokio::test]
    async fn supplies_json_stdin_turn_environment_and_working_directory() {
        let tools = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let bin = script(tools.path(), r#"printf '%s\n%s\n' "$GLLMH_TURN" "$PWD"; cat"#);
        let turn = Ulid::new().to_string();
        let mut input = input(cwd.path());
        input.env.insert(ENV_TURN.into(), turn.clone());
        let expected = format!("{turn}\n{}\n{}", cwd.path().display(), input.arguments);

        let output = run(&bin, input).await.unwrap();
        assert_eq!(output, RunOutput { exit_code: 0, stdout: expected, stderr: String::new() });
    }

    #[tokio::test]
    async fn returns_exit_status_stdout_and_stderr_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "printf partial; printf failure >&2; exit 7");
        let output = run(&bin, input(dir.path())).await.unwrap();
        assert_eq!(output, RunOutput { exit_code: 7, stdout: "partial".into(), stderr: "failure".into() });
    }

    #[tokio::test]
    async fn reports_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("no-exec");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = run(&bin, input(dir.path())).await.unwrap_err();
        assert!(matches!(err, RunError::NotExecutable { bin: path, .. } if path == bin));
    }

    #[tokio::test]
    async fn timeout_kills_the_group_even_after_the_leader_exits() {
        let dir = tempfile::tempdir().unwrap();
        // The background child keeps the pipes open after the shell exits.
        let bin = script(dir.path(), "sleep 30 &\nprintf '%s' \"$!\" > child-pid\nexit 0");
        let mut input = input(dir.path());
        input.timeout_s = 1;
        let err = run(&bin, input).await.unwrap_err();
        assert!(matches!(err, RunError::Timeout { timeout_s: 1 }));
        let pid = std::fs::read_to_string(dir.path().join("child-pid")).unwrap();
        let stat_path = format!("/proc/{pid}/stat");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match tokio::fs::read_to_string(&stat_path).await {
                    Ok(stat) if stat.rsplit_once(") ").unwrap().1.starts_with('Z') => break,
                    Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    Err(e) => panic!("cannot read {stat_path}: {e}"),
                }
            }
        })
        .await
        .expect("descendant was killed");
    }
}
