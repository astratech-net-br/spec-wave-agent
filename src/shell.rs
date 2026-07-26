//! Execução de comandos externos + retry com backoff.

use anyhow::{bail, Context, Result};
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<Out> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null());
    if program == "git" {
        // Falha rápida em vez de travar pedindo credencial (systemd/launchd).
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
    let output = cmd
        .output()
        .await
        .with_context(|| format!("falha ao executar {program} {args:?}"))?;
    Ok(Out {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub async fn run_ok(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = run(cwd, program, args).await?;
    if !out.ok {
        bail!("{program} {args:?} falhou:\n{}{}", out.stdout, out.stderr);
    }
    Ok(out.stdout)
}

/// Retry com backoff exponencial (base, 2x, 4x, ...) para operações de
/// leitura/rede idempotentes. NÃO usar nas operações de CAS do lease:
/// lá, repetir um push cegamente pode mascarar uma perda de lease.
pub async fn retry_backoff<T, F, Fut>(
    what: &str,
    attempts: u32,
    base: Duration,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut delay = base;
    let mut last_err = None;
    for attempt in 1..=attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt < attempts {
                    tracing::warn!(target: "agent",
                        "{what}: tentativa {attempt}/{attempts} falhou: {e:#}; \
                         nova em {delay:?}");
                    sleep(delay).await;
                    delay *= 2;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}
