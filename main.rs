//! spec-wave-agent — daemon para máquina de dev que puxa Stories/Tasks da
//! fila (label `agent:queued`), garante exclusão mútua entre múltiplos
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
//!
//! Autenticação: usa o `git`/`gh` já configurados na máquina do dev.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Duration as TokioDuration};

// ---------------------------------------------------------------------------
// Config (~/.config/spec-wave-agent/config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Config {
    /// "owner/repo"
    repo: String,
    /// Label que marca itens na fila (aplicada por humano ou automação do board)
    #[serde(default = "d_queue_label")]
    queue_label: String,
    #[serde(default = "d_poll")]
    poll_interval_secs: u64,
    #[serde(default = "d_heartbeat")]
    heartbeat_secs: u64,
    /// Sem heartbeat por este tempo => lease considerado morto (pode roubar)
    #[serde(default = "d_ttl")]
    lease_ttl_secs: i64,
    #[serde(default = "d_impl_timeout")]
    implement_timeout_secs: u64,
    /// Diretório de trabalho do agente
    #[serde(default = "d_workdir")]
    workdir: String,
    /// Identidade do agente (default: usuario@hostname)
    agent_id: Option<String>,
}

fn d_queue_label() -> String { "agent:queued".into() }
fn d_poll() -> u64 { 60 }
fn d_heartbeat() -> u64 { 120 }
fn d_ttl() -> i64 { 600 }
fn d_impl_timeout() -> u64 { 3600 }
fn d_workdir() -> String {
    dirs_home().join(".spec-wave-agent").to_string_lossy().into_owned()
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_config() -> Result<Config> {
    let path = dirs_home().join(".config/spec-wave-agent/config.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("config não encontrada em {}", path.display()))?;
    Ok(toml::from_str(&raw)?)
}

fn agent_id(cfg: &Config) -> String {
    cfg.agent_id.clone().unwrap_or_else(|| {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "agent".into());
        let host = hostname();
        format!("{user}@{host}")
    })
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".into())
}

// ---------------------------------------------------------------------------
// Shell helpers
// ---------------------------------------------------------------------------

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<Out> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("falha ao executar {program} {args:?}"))?;
    Ok(Out {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn run_ok(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = run(cwd, program, args).await?;
    if !out.ok {
        bail!("{program} {args:?} falhou:\n{}{}", out.stdout, out.stderr);
    }
    Ok(out.stdout)
}

// ---------------------------------------------------------------------------
// Lease distribuído sobre git refs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lease {
    issue: u64,
    owner: String,
    /// Fencing token: incrementa a cada aquisição/roubo
    generation: u64,
    heartbeat: DateTime<Utc>,
}

struct LeaseRepo {
    dir: PathBuf,
    remote_url: String,
}

impl LeaseRepo {
    async fn open(cfg: &Config) -> Result<Self> {
        let dir = PathBuf::from(&cfg.workdir).join("lease-repo");
        let remote_url = format!("https://github.com/{}.git", cfg.repo);
        if !dir.join(".git").exists() {
            std::fs::create_dir_all(&dir)?;
            run_ok(&dir, "git", &["init", "--quiet"]).await?;
            run_ok(&dir, "git", &["remote", "add", "origin", &remote_url]).await?;
        }
        Ok(Self { dir, remote_url })
    }

    fn claim_ref(issue: u64) -> String {
        format!("refs/heads/spec-wave-agent/claims/{issue}")
    }

    /// Lê o lease atual do remoto. Retorna (sha_da_ref, lease) ou None.
    async fn fetch(&self, issue: u64) -> Result<Option<(String, Lease)>> {
        let r = Self::claim_ref(issue);
        let ls = run(&self.dir, "git", &["ls-remote", "origin", &r]).await?;
        let sha = ls.stdout.split_whitespace().next().unwrap_or("").to_string();
        if sha.is_empty() {
            return Ok(None);
        }
        run_ok(&self.dir, "git",
               &["fetch", "--quiet", "origin", &format!("+{r}:{r}")]).await?;
        let content = run_ok(&self.dir, "git",
                             &["show", &format!("{sha}:lease.json")]).await?;
        let lease: Lease = serde_json::from_str(content.trim())
            .context("lease.json inválido")?;
        Ok(Some((sha, lease)))
    }

    /// Cria um commit contendo apenas lease.json (git plumbing, sem worktree).
    async fn make_commit(&self, lease: &Lease, parent: Option<&str>) -> Result<String> {
        let json = serde_json::to_string_pretty(lease)?;
        // blob
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        {
            use tokio::io::AsyncWriteExt;
            child.stdin.take().unwrap().write_all(json.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("git hash-object falhou");
        }
        let blob = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // tree
        let mut child = Command::new("git")
            .args(["mktree"])
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        {
            use tokio::io::AsyncWriteExt;
            let line = format!("100644 blob {blob}\tlease.json\n");
            child.stdin.take().unwrap().write_all(line.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("git mktree falhou");
        }
        let tree = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // commit
        let msg = format!("lease issue #{} gen {} by {}",
                          lease.issue, lease.generation, lease.owner);
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), msg];
        if let Some(p) = parent {
            args.push("-p".into());
            args.push(p.into());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let sha = run_ok(&self.dir, "git", &argv).await?;
        Ok(sha.trim().to_string())
    }

    /// Tenta adquirir o lease. Retorna Some(lease) se este agente é o dono.
    async fn try_acquire(&self, issue: u64, owner: &str, ttl: i64)
        -> Result<Option<Lease>>
    {
        let r = Self::claim_ref(issue);
        match self.fetch(issue).await? {
            None => {
                // Ref não existe: push NÃO-forçado = CAS de criação.
                let lease = Lease {
                    issue, owner: owner.into(), generation: 1,
                    heartbeat: Utc::now(),
                };
                let sha = self.make_commit(&lease, None).await?;
                let push = run(&self.dir, "git",
                               &["push", "--quiet", "origin",
                                 &format!("{sha}:{r}")]).await?;
                Ok(push.ok.then_some(lease)) // perdeu a corrida => None
            }
            Some((cur_sha, cur)) => {
                let expired = Utc::now() - cur.heartbeat
                    > Duration::seconds(ttl);
                if !expired {
                    return Ok(None); // dono vivo
                }
                // STEAL: force-with-lease esperando exatamente o sha estagnado.
                let lease = Lease {
                    issue, owner: owner.into(),
                    generation: cur.generation + 1,
                    heartbeat: Utc::now(),
                };
                let sha = self.make_commit(&lease, Some(&cur_sha)).await?;
                let spec = format!("--force-with-lease={r}:{cur_sha}");
                let push = run(&self.dir, "git",
                               &["push", "--quiet", &spec, "origin",
                                 &format!("{sha}:{r}")]).await?;
                if push.ok {
                    eprintln!("[lease] roubado issue #{issue} gen {} \
                               (dono anterior: {}, parado desde {})",
                              lease.generation, cur.owner, cur.heartbeat);
                }
                Ok(push.ok.then_some(lease))
            }
        }
    }

    /// Renova o heartbeat via CAS. Err => PERDEMOS o lease (abortar tudo).
    async fn renew(&self, lease: &mut Lease) -> Result<()> {
        let r = Self::claim_ref(lease.issue);
        let (cur_sha, cur) = self.fetch(lease.issue).await?
            .ok_or_else(|| anyhow!("lease sumiu"))?;
        if cur.owner != lease.owner || cur.generation != lease.generation {
            bail!("lease tomado por {} (gen {})", cur.owner, cur.generation);
        }
        lease.heartbeat = Utc::now();
        let sha = self.make_commit(lease, Some(&cur_sha)).await?;
        let spec = format!("--force-with-lease={r}:{cur_sha}");
        let push = run(&self.dir, "git",
                       &["push", "--quiet", &spec, "origin",
                         &format!("{sha}:{r}")]).await?;
        if !push.ok {
            bail!("renew rejeitado (CAS): lease perdido");
        }
        Ok(())
    }

    async fn release(&self, issue: u64) -> Result<()> {
        let r = Self::claim_ref(issue);
        let _ = run(&self.dir, "git",
                    &["push", "--quiet", "origin", &format!(":{r}")]).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fila (labels = UX; a correção está no lease)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhLabel { name: String }
#[derive(Deserialize)]
struct GhIssue { number: u64, labels: Vec<GhLabel> }

async fn poll_queue(cfg: &Config, cwd: &Path) -> Result<Vec<u64>> {
    let out = run_ok(cwd, "gh",
        &["issue", "list", "--repo", &cfg.repo,
          "--label", &cfg.queue_label, "--state", "open",
          "--json", "number,labels", "--limit", "50"]).await?;
    let issues: Vec<GhIssue> = serde_json::from_str(&out)?;
    let mut nums: Vec<u64> = issues.into_iter()
        .filter(|i| i.labels.iter()
            .any(|l| l.name == "[STORY]" || l.name == "[TASK]"))
        .map(|i| i.number)
        .collect();
    nums.sort_unstable(); // FIFO por número
    Ok(nums)
}

// ---------------------------------------------------------------------------
// Workspace + execução do spec-wave implement
// ---------------------------------------------------------------------------

async fn ensure_workspace(cfg: &Config, issue: u64) -> Result<PathBuf> {
    let ws = PathBuf::from(&cfg.workdir).join(format!("issue-{issue}"));
    let url = format!("https://github.com/{}.git", cfg.repo);
    if !ws.join(".git").exists() {
        std::fs::create_dir_all(ws.parent().unwrap())?;
        run_ok(Path::new("."), "git",
               &["clone", "--quiet", &url, ws.to_str().unwrap()]).await?;
    } else {
        run_ok(&ws, "git", &["fetch", "--quiet", "origin"]).await?;
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
        eprintln!("[ws] retomando branch existente {branch}");
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
async fn checkpoint(ws: &Path, issue: u64, label: &str) {
    let _ = run(ws, "git", &["add", "-A"]).await;
    let _ = run(ws, "git",
                &["commit", "-m",
                  &format!("wip: checkpoint #{issue} ({label}) [spec-wave-agent]")])
        .await; // pode falhar se não houver mudanças: ok
    let _ = run(ws, "git", &["push", "--quiet"]).await;
}

enum RunEnd { Success, Failed(String), LeaseLost, Interrupted }

async fn implement(
    cfg: &Config, ws: &Path, issue: u64,
    mut lease_lost: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<RunEnd> {
    let mut child = Command::new("npx")
        .args(["spec-wave", "implement", &issue.to_string()])
        .current_dir(ws)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("falha ao iniciar npx spec-wave implement")?;

    let cap = TokioDuration::from_secs(cfg.implement_timeout_secs);
    tokio::select! {
        status = timeout(cap, child.wait()) => match status {
            Err(_) => {
                let _ = child.kill().await;
                Ok(RunEnd::Failed(format!("timeout de {}s", cap.as_secs())))
            }
            Ok(Ok(st)) if st.success() => Ok(RunEnd::Success),
            Ok(Ok(st)) => Ok(RunEnd::Failed(format!("exit code {st}"))),
            Ok(Err(e)) => Ok(RunEnd::Failed(e.to_string())),
        },
        _ = lease_lost.changed() => {
            // Fencing: perdemos o lease => outro agente pode estar ativo.
            // Matar imediatamente, SEM push (o novo dono manda no branch).
            let _ = child.kill().await;
            Ok(RunEnd::LeaseLost)
        }
        _ = shutdown.changed() => {
            let _ = child.kill().await;
            Ok(RunEnd::Interrupted)
        }
    }
}

// ---------------------------------------------------------------------------
// Processamento de uma issue (com lease + heartbeat)
// ---------------------------------------------------------------------------

async fn process_issue(
    cfg: &Config, leases: &LeaseRepo, me: &str, issue: u64,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let Some(lease) = leases
        .try_acquire(issue, me, cfg.lease_ttl_secs).await? else {
        return Ok(()); // outro agente pegou: segue a vida
    };
    eprintln!("[agent] claim OK: issue #{issue} (gen {})", lease.generation);

    let (lost_tx, lost_rx) = watch::channel(false);

    // Heartbeat em background; ao falhar, sinaliza fencing.
    let hb_lease = lease.clone();
    let hb_repo_dir = leases.dir.clone();
    let hb_remote = leases.remote_url.clone();
    let hb_secs = cfg.heartbeat_secs;
    let hb = tokio::spawn(async move {
        let repo = LeaseRepo { dir: hb_repo_dir, remote_url: hb_remote };
        let mut l = hb_lease;
        loop {
            sleep(TokioDuration::from_secs(hb_secs)).await;
            if let Err(e) = repo.renew(&mut l).await {
                eprintln!("[lease] renew falhou: {e}");
                let _ = lost_tx.send(true);
                break;
            }
        }
    });

    let ws = ensure_workspace(cfg, issue).await?;
    let end = implement(cfg, &ws, issue, lost_rx, shutdown).await?;
    hb.abort();

    match end {
        RunEnd::Success => {
            // Sai da fila (label = UX) e libera o lease.
            let _ = run(&ws, "gh",
                &["issue", "edit", &issue.to_string(), "--repo", &cfg.repo,
                  "--remove-label", &cfg.queue_label]).await;
            leases.release(issue).await?;
            eprintln!("[agent] issue #{issue} concluída");
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
        }
        RunEnd::Interrupted => {
            // Dev desligando a máquina: checkpoint + release imediato para
            // takeover instantâneo (sem esperar TTL).
            checkpoint(&ws, issue, "interrupted").await;
            leases.release(issue).await?;
            eprintln!("[agent] issue #{issue} interrompida; lease liberado");
        }
        RunEnd::LeaseLost => {
            // NÃO faz checkpoint nem release: não somos mais donos de nada.
            eprintln!("[agent] issue #{issue}: lease perdido, abortado");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = load_config()?;
    let me = agent_id(&cfg);
    std::fs::create_dir_all(&cfg.workdir)?;
    let leases = LeaseRepo::open(&cfg).await?;
    eprintln!("[agent] {me} iniciando; repo {} fila '{}'",
              cfg.repo, cfg.queue_label);

    // Shutdown gracioso (SIGTERM do systemd/launchd, Ctrl+C)
    let (sd_tx, sd_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        eprintln!("[agent] sinal de shutdown recebido");
        let _ = sd_tx.send(true);
    });

    loop {
        if *sd_rx.borrow() {
            break;
        }
        match poll_queue(&cfg, Path::new(&cfg.workdir)).await {
            Ok(queue) if !queue.is_empty() => {
                // 1 issue por vez, por agente (max_concurrent = 1)
                for issue in queue {
                    if *sd_rx.borrow() { break; }
                    if let Err(e) = process_issue(
                        &cfg, &leases, &me, issue, sd_rx.clone()).await {
                        eprintln!("[agent] erro na issue #{issue}: {e:#}");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("[agent] poll falhou: {e:#}"),
        }
        tokio::select! {
            _ = sleep(TokioDuration::from_secs(cfg.poll_interval_secs)) => {}
            _ = wait_true(sd_rx.clone()) => break,
        }
    }
    eprintln!("[agent] encerrado");
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
