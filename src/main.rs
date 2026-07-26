//! spec-wave-agent — daemon para máquina de dev que puxa Stories/Tasks da
//! fila (label `spec-wave:dev-agent`), garante exclusão mútua entre múltiplos
//! agentes via lease em git refs (CAS real), e delega a implementação ao
//! `npx spec-wave implement` (que invoca o Claude Code do próprio dev).
//!
//! Propriedades:
//!   - Claim atômico: criar a ref de lease é CAS (push não-forçado falha se
//!     a ref existe; renovação/roubo usam --force-with-lease).
//!   - Resiliência: heartbeat a cada `heartbeat_secs`; se o dono sumir por
//!     mais que `lease_ttl_secs` (máquina desligada), outro agente rouba o
//!     lease e retoma do branch de trabalho (checkpoint = commits pushados).
//!   - Fencing: se o heartbeat falhar (lease perdido), o child é morto na
//!     hora para evitar dois agentes trabalhando na mesma issue.
//!   - Uma issue por vez: só depois de terminar uma issue o agente volta à
//!     fila (re-poll fresco) para tentar obter a próxima.
//!
//! Autenticação: usa o `git`/`gh` já configurados na máquina do dev.

mod config;
mod lease;
mod queue;
mod runner;
mod shell;

use anyhow::{bail, Result};
use config::{load_config, Config};
use lease::{Lease, LeaseRepo, RenewError};
use runner::{checkpoint, ensure_workspace, implement, RunEnd};
use shell::run;
use std::path::Path;
use std::time::Instant;
use tokio::sync::watch;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{error, info, warn};

fn init_tracing() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Checagens fail-fast dos pré-requisitos da máquina.
async fn preflight(cwd: &Path) -> Result<()> {
    if !run(cwd, "git", &["--version"]).await.map(|o| o.ok).unwrap_or(false) {
        bail!("git não encontrado no PATH");
    }
    if !run(cwd, "npx", &["--version"]).await.map(|o| o.ok).unwrap_or(false) {
        bail!("npx não encontrado no PATH — instale Node 18+");
    }
    match run(cwd, "gh", &["auth", "status"]).await {
        Ok(o) if o.ok => {}
        Ok(o) => bail!("gh não autenticado — rode `gh auth login`:\n{}",
                       o.stderr.trim()),
        Err(_) => bail!("gh não encontrado no PATH — instale o GitHub CLI"),
    }
    Ok(())
}

/// Heartbeat em background. Lost => fencing imediato; Transient => retry
/// dentro de um orçamento que garante auto-fencing ANTES de qualquer roubo
/// legal: um roubo exige `ttl` sem heartbeat GRAVADO no remoto, e nós nos
/// cercamos em `ttl − 2×heartbeat` após o último renew bem-sucedido
/// (validate() garante ttl >= 4×hb, logo orçamento >= 2×hb > 0).
fn spawn_heartbeat(
    repo: LeaseRepo, mut lease: Lease, hb_secs: u64, ttl_secs: i64,
    lost_tx: watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let budget = std::time::Duration::from_secs(
            (ttl_secs as u64).saturating_sub(2 * hb_secs));
        let mut last_ok = Instant::now();
        loop {
            sleep(TokioDuration::from_secs(hb_secs)).await;
            loop {
                match repo.renew(&mut lease).await {
                    Ok(()) => { last_ok = Instant::now(); break; }
                    Err(RenewError::Lost(m)) => {
                        error!(target: "lease", "renew: lease perdido: {m}");
                        let _ = lost_tx.send(true);
                        return;
                    }
                    Err(RenewError::Transient(e)) => {
                        if last_ok.elapsed() >= budget {
                            error!(target: "lease",
                                "renew sem sucesso há {:?} (orçamento {:?}): \
                                 auto-fencing conservador",
                                last_ok.elapsed(), budget);
                            let _ = lost_tx.send(true);
                            return;
                        }
                        warn!(target: "lease",
                              "renew transiente: {e:#}; nova tentativa em 10s");
                        sleep(TokioDuration::from_secs(10)).await;
                    }
                }
            }
        }
    })
}

/// Processa uma issue. Retorna true se este agente claimou (e portanto a
/// fila deve ser re-consultada fresca), false se outro agente ficou com ela.
async fn process_issue(
    cfg: &Config, leases: &LeaseRepo, me: &str, issue: u64,
    shutdown: watch::Receiver<bool>,
) -> Result<bool> {
    let Some(lease) = leases
        .try_acquire(issue, me, cfg.lease_ttl_secs).await? else {
        return Ok(false); // outro agente pegou: segue a vida
    };
    info!(target: "agent", "claim OK: issue #{issue} (gen {})", lease.generation);

    let (lost_tx, lost_rx) = watch::channel(false);
    let hb = spawn_heartbeat(leases.clone(), lease, cfg.heartbeat_secs,
                             cfg.lease_ttl_secs, lost_tx);

    let outcome = async {
        let ws = ensure_workspace(cfg, issue).await?;
        let end = implement(cfg, &ws, issue, lost_rx.clone(), shutdown).await?;
        Ok::<_, anyhow::Error>((ws, end))
    }.await;
    hb.abort();

    let (ws, end) = match outcome {
        Ok(v) => v,
        Err(e) => {
            // Falha de infra (workspace/spawn): devolve o lease para não
            // prender a issue — mas só se ele ainda for nosso.
            if !*lost_rx.borrow() {
                let _ = leases.release(issue).await;
            }
            return Err(e);
        }
    };

    match end {
        RunEnd::Success => {
            // Sai da fila (label = UX) e libera o lease.
            let _ = run(&ws, "gh",
                &["issue", "edit", &issue.to_string(), "--repo", &cfg.repo,
                  "--remove-label", &cfg.queue_label]).await;
            leases.release(issue).await?;
            info!(target: "agent", "issue #{issue} concluída");
        }
        RunEnd::Failed(reason) => {
            checkpoint(&ws, issue, "failed").await;
            let _ = run(&ws, "gh",
                &["issue", "comment", &issue.to_string(), "--repo", &cfg.repo,
                  "--body",
                  &format!("⚠️ Agente `{me}` falhou: {reason}. Checkpoint \
                            pushado no branch — outro agente (ou humano) \
                            pode retomar. Item permanece na fila.")]).await;
            leases.release(issue).await?; // devolve p/ fila: label continua
            warn!(target: "agent", "issue #{issue} falhou: {reason}");
        }
        RunEnd::Interrupted => {
            // Dev desligando a máquina: checkpoint + release imediato para
            // takeover instantâneo (sem esperar TTL).
            checkpoint(&ws, issue, "interrupted").await;
            leases.release(issue).await?;
            info!(target: "agent", "issue #{issue} interrompida; lease liberado");
        }
        RunEnd::LeaseLost => {
            // NÃO faz checkpoint nem release: não somos mais donos de nada.
            warn!(target: "agent", "issue #{issue}: lease perdido, abortado");
        }
    }
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = load_config()?;
    cfg.validate()?;
    let me = cfg.agent_id();
    std::fs::create_dir_all(&cfg.workdir)?;
    let workdir = Path::new(&cfg.workdir).to_path_buf();
    preflight(&workdir).await?;
    let leases = LeaseRepo::open(
        workdir.join("lease-repo"), &cfg.remote_url()).await?;
    info!(target: "agent", "{me} iniciando; repo {} fila '{}'",
          cfg.repo, cfg.queue_label);

    // Shutdown gracioso (SIGTERM do systemd/launchd, Ctrl+C)
    let (sd_tx, sd_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!(target: "agent", "sinal de shutdown recebido");
        let _ = sd_tx.send(true);
    });

    loop {
        if *sd_rx.borrow() {
            break;
        }
        let mut claimed = false;
        match queue::poll_queue(&cfg, &workdir).await {
            Ok(queue) => {
                // Uma issue por vez: pega a PRIMEIRA que conseguir claimar;
                // ao terminar, volta direto ao poll (fila fresca).
                for issue in queue {
                    if *sd_rx.borrow() { break; }
                    match process_issue(&cfg, &leases, &me, issue,
                                        sd_rx.clone()).await {
                        Ok(true) => { claimed = true; break; }
                        Ok(false) => {} // outro agente: tenta a próxima
                        Err(e) => {
                            error!(target: "agent", "erro na issue #{issue}: {e:#}");
                            claimed = true; // pode ter claimado: re-poll fresco
                            break;
                        }
                    }
                }
            }
            Err(e) => error!(target: "agent", "poll falhou: {e:#}"),
        }
        if claimed {
            continue; // terminou um fluxo => tenta obter a próxima já
        }
        tokio::select! {
            _ = sleep(TokioDuration::from_secs(cfg.poll_interval_secs)) => {}
            _ = wait_true(sd_rx.clone()) => break,
        }
    }
    info!(target: "agent", "encerrado");
    Ok(())
}

async fn wait_true(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() { return; }
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
