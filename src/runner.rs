//! Workspace por issue + execução do `npx spec-wave implement` com
//! streaming do output do filho para o console do agente.

use crate::config::Config;
use crate::shell::{retry_backoff, run, run_ok};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{timeout, Duration as TokioDuration};
use tracing::info;

pub async fn ensure_workspace(cfg: &Config, issue: u64) -> Result<PathBuf> {
    let ws = PathBuf::from(&cfg.workdir).join(format!("issue-{issue}"));
    let url = cfg.remote_url();
    if !ws.join(".git").exists() {
        std::fs::create_dir_all(ws.parent().unwrap())?;
        retry_backoff("clone do workspace", 3, Duration::from_secs(2), || {
            let url = url.clone();
            let ws = ws.clone();
            async move {
                run_ok(Path::new("."), "git",
                       &["clone", "--quiet", &url, ws.to_str().unwrap()]).await
            }
        }).await?;
    } else {
        retry_backoff("fetch do workspace", 3, Duration::from_secs(2), || {
            let ws = ws.clone();
            async move {
                run_ok(&ws, "git", &["fetch", "--quiet", "origin"]).await
            }
        }).await?;
    }

    // Retomada: se o branch de trabalho já existe no remoto (outro agente
    // começou e caiu), continua dele; senão cria a partir do default.
    let branch = format!("agent/issue-{issue}");
    let remote = run(&ws, "git",
                     &["ls-remote", "--exit-code", "origin",
                       &format!("refs/heads/{branch}")]).await?;
    if remote.ok {
        run_ok(&ws, "git", &["checkout", "-B", &branch,
                             &format!("origin/{branch}")]).await?;
        info!(target: "ws", "retomando branch existente {branch}");
    } else {
        let head = run_ok(&ws, "git",
            &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"]).await
            .unwrap_or_else(|_| "origin/main".into());
        run_ok(&ws, "git", &["checkout", "-B", &branch, head.trim()]).await?;
        run_ok(&ws, "git", &["push", "--quiet", "-u", "origin", &branch]).await?;
    }
    Ok(ws)
}

/// Commita e pusha qualquer estado pendente (checkpoint para takeover).
pub async fn checkpoint(ws: &Path, issue: u64, label: &str) {
    let _ = run(ws, "git", &["add", "-A"]).await;
    let _ = run(ws, "git",
                &["commit", "-m",
                  &format!("wip: checkpoint #{issue} ({label}) [spec-wave-agent]")])
        .await; // pode falhar se não houver mudanças: ok
    let _ = run(ws, "git", &["push", "--quiet"]).await;
}

pub enum RunEnd { Success, Failed(String), LeaseLost, Interrupted }

async fn stream_lines<R>(reader: R, issue: u64, stream: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        info!(target: "spec-wave", "[#{issue}][{stream}] {line}");
    }
}

pub async fn implement(
    cfg: &Config, ws: &Path, issue: u64,
    mut lease_lost: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<RunEnd> {
    let mut child = Command::new("npx")
        .args(["spec-wave", "implement", &issue.to_string()])
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("falha ao iniciar npx spec-wave implement")?;

    // Os handles saem do child via take(), então child.kill() nos braços
    // abaixo continua imediato — fencing não espera os readers.
    let out_task = child.stdout.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "out")));
    let err_task = child.stderr.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "err")));

    let cap = TokioDuration::from_secs(cfg.implement_timeout_secs);
    let end = tokio::select! {
        status = timeout(cap, child.wait()) => match status {
            Err(_) => {
                let _ = child.kill().await;
                RunEnd::Failed(format!("timeout de {}s", cap.as_secs()))
            }
            Ok(Ok(st)) if st.success() => RunEnd::Success,
            Ok(Ok(st)) => RunEnd::Failed(format!("exit code {st}")),
            Ok(Err(e)) => RunEnd::Failed(e.to_string()),
        },
        _ = lease_lost.changed() => {
            // Fencing: perdemos o lease => outro agente pode estar ativo.
            // Matar imediatamente, SEM push (o novo dono manda no branch).
            let _ = child.kill().await;
            RunEnd::LeaseLost
        }
        _ = shutdown.changed() => {
            let _ = child.kill().await;
            RunEnd::Interrupted
        }
    };

    // Drena o resto do output (após kill/exit os pipes fecham em EOF).
    for t in [out_task, err_task].into_iter().flatten() {
        let _ = timeout(TokioDuration::from_secs(2), t).await;
    }
    Ok(end)
}
