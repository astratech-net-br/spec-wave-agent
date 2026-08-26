//! spec-wave-agent — daemon para máquina de dev que puxa FEATURES da
//! fila (label `spec-wave:dev-agent` em issues [FEATURE]), garante exclusão
//! mútua entre múltiplos agentes via lease em git refs (CAS real), e delega
//! a orquestração ao Claude Code: o prompt (config `feature_prompt`) instrui
//! usar `npx spec-wave order`/`implement` para implementar todas as user
//! stories da feature na ordem de dependência, paralelizando com sub-agentes.
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

use anyhow::{bail, Result};
use spec_wave_agent::config::{load_config_from, Config};
use spec_wave_agent::lease::{Lease, LeaseRepo, RenewError};
use spec_wave_agent::queue::{self, QueueItem, QueueKind};
use spec_wave_agent::runner::{checkpoint, ensure_workspace, open_pull_request, run_item, RunEnd};
use spec_wave_agent::shell::run;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
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

/// Desfecho de uma tentativa, do ponto de vista do LOOP.
///
/// `failed` existe para o loop contar falhas seguidas na mesma issue: falha
/// mantém a label (segue na fila), e sem teto uma falha determinística é
/// re-tentada para sempre.
#[derive(Debug, Clone, Copy)]
struct Attempt { claimed: bool, failed: bool }

/// Processa uma issue. `claimed` = este agente pegou a issue (e portanto a
/// fila deve ser re-consultada fresca); false = outro agente ficou com ela.
async fn process_issue(
    cfg: &Config, leases: &LeaseRepo, me: &str, item: QueueItem,
    shutdown: watch::Receiver<bool>,
) -> Result<Attempt> {
    let issue = item.number;
    // "bug"/"feature" nas mensagens: com dois tipos na fila, um log que diz só
    // "issue #42" obriga a abrir o GitHub para saber o que o agente está fazendo.
    let que = match item.kind { QueueKind::Bug => "bug", QueueKind::Feature => "feature" };
    let Some(lease) = leases
        .try_acquire(issue, me, cfg.lease_ttl_secs).await? else {
        return Ok(Attempt { claimed: false, failed: false }); // outro agente pegou
    };
    info!(target: "agent", "claim OK: issue #{issue} (gen {})", lease.generation);

    let (lost_tx, lost_rx) = watch::channel(false);
    let hb = spawn_heartbeat(leases.clone(), lease, cfg.heartbeat_secs,
                             cfg.lease_ttl_secs, lost_tx);

    let outcome = async {
        let ws = ensure_workspace(cfg, issue).await?;
        let end = run_item(cfg, &ws, item, lost_rx.clone(), shutdown).await?;
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

    let mut failed = false;
    match end {
        RunEnd::Success(result) => {
            // Cinto de segurança: garante push de qualquer resto que o
            // executor não tenha commitado, antes de sair da fila.
            checkpoint(&ws, issue, "success").await;
            // Num bug, o VALOR do trabalho está na causa raiz: sem publicá-la
            // na issue, ela morre no marker (que nem é commitado) e a próxima
            // pessoa reinvestiga o mesmo defeito.
            if item.kind == QueueKind::Bug {
                if let Some(body) = render_bug_report(&result, me) {
                    let _ = run(&ws, "gh",
                        &["issue", "comment", &issue.to_string(),
                          "--repo", &cfg.repo, "--body", &body]).await;
                }
            }
            // O PR vem DEPOIS do checkpoint (todo o trabalho já está pushado) e
            // ANTES de sair da fila: é ele que dá conteúdo à etapa 👀 Code
            // Review. Sem isso, as Stories chegavam lá e a fila do Tech Leader
            // mostrava "sem PR" em todas.
            open_pull_request(&ws, &cfg.repo, issue, cfg.pr_draft).await;
            let _ = run(&ws, "gh",
                &["issue", "edit", &issue.to_string(), "--repo", &cfg.repo,
                  "--remove-label", &cfg.queue_label]).await;
            leases.release(issue).await?;
            info!(target: "agent", "{que} #{issue} concluído");
        }
        RunEnd::Failed(reason) => {
            checkpoint(&ws, issue, "failed").await;
            let _ = run(&ws, "gh",
                &["issue", "comment", &issue.to_string(), "--repo", &cfg.repo,
                  "--body",
                  &format!("⚠️ Agente `{me}` falhou: {reason}. Checkpoint \
                            pushado no branch — outro agente (ou humano) \
                            pode retomar; rode `npx spec-wave order {issue}` \
                            para ver o estado das stories. Item permanece \
                            na fila.")]).await;
            leases.release(issue).await?; // devolve p/ fila: label continua
            failed = true;
            warn!(target: "agent", "{que} #{issue} falhou: {reason}");
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
    Ok(Attempt { claimed: true, failed })
}

/// Tira a issue da fila depois de falhas seguidas demais e explica na issue.
///
/// Sem isto, uma falha DETERMINÍSTICA (o executor concluir que não há como
/// reproduzir, por exemplo) é re-claimada a cada cooldown para sempre —
/// pagando um executor inteiro por tentativa, indefinidamente. Manter a label
/// só faz sentido enquanto a falha puder ser transitória.
async fn desistir_da_issue(cfg: &Config, cwd: &Path, me: &str, issue: u64, tentativas: u32) {
    let _ = run(cwd, "gh",
        &["issue", "comment", &issue.to_string(), "--repo", &cfg.repo,
          "--body",
          // Item 2 do rfc/plano-hardening-agentes-2026-08.md: este format!
          // era UMA linha física sem `\` de continuação, e os espaços de
          // indentação do editor viravam corridas de espaços literais no
          // texto publicado na issue. Com `\`, cada quebra some junto com o
          // espaço em branco que a segue.
          &format!("🛑 Agente `{me}` desistiu de #{issue} após **{tentativas} falhas \
                    seguidas** — as tentativas anteriores estão nos comentários acima. \
                    A label `{}` foi REMOVIDA para parar o ciclo de retentativa: falha \
                    que se repete não é transitória, e continuar tentando só gasta \
                    execução. Uma pessoa precisa resolver o que está bloqueando e \
                    reaplicar a label para reenfileirar.", cfg.queue_label)]).await;
    let _ = run(cwd, "gh",
        &["issue", "edit", &issue.to_string(), "--repo", &cfg.repo,
          "--remove-label", &cfg.queue_label]).await;
    warn!(target: "agent",
          "issue #{issue}: {tentativas} falhas seguidas — removida da fila");
}


/// Comentário de conclusão de um Bug a partir do marker.
///
/// `None` quando o executor não preencheu nada de RCA — um comentário só com
/// "concluído" é ruído, e a ausência já é informação (o prompt pede os campos).
fn render_bug_report(result: &Option<spec_wave_agent::runner::ExecResult>, me: &str)
    -> Option<String>
{
    let r = result.as_ref()?;
    let mut linhas = Vec::new();
    if let Some(c) = r.causa_raiz.as_deref().filter(|s| !s.trim().is_empty()) {
        linhas.push(format!("**Causa raiz:** {c}"));
    }
    if let Some(v) = r.fix.as_deref().filter(|s| !s.trim().is_empty()) {
        linhas.push(format!("**Escopo do fix:** {v}"));
    }
    if let Some(t) = r.teste_regressao.as_deref().filter(|s| !s.trim().is_empty()) {
        linhas.push(format!("**Teste de regressão:** {t}"));
    }
    if let Some(a) = r.arquivos.as_ref().filter(|a| !a.is_empty()) {
        linhas.push(format!("**Arquivos:** {}", a.join(", ")));
    }
    if linhas.is_empty() {
        return None;
    }
    Some(format!("🐞 **Fix implementado pelo agente `{me}`**\n\n{}", linhas.join("\n\n")))
}

const USAGE: &str = "\
spec-wave-agent — daemon que puxa FEATURES e BUGS da fila do GitHub
(label `spec-wave:dev-agent`) e delega a implementação ao Claude Code.

USO:
    spec-wave-agent [OPÇÕES]

OPÇÕES:
    -c, --config <PATH>  Arquivo de configuração TOML.
                         Default: $SPEC_WAVE_AGENT_CONFIG, ou
                         ~/.config/spec-wave-agent/config.toml
    -V, --version        Imprime a versão e sai
    -h, --help           Imprime esta ajuda e sai

Sem opções, o agente roda em foreground até Ctrl+C (que faz checkpoint do
trabalho em andamento e libera o lease). Log via RUST_LOG (ex.: RUST_LOG=debug).";

/// Opções da linha de comando. Um parser à mão em vez de clap: são três
/// flags, e a dependência não se paga.
struct Args {
    config: Option<String>,
}

/// `Ok(None)` = já respondeu (--help/--version) e o processo deve sair.
fn parse_args() -> Result<Option<Args>> {
    let mut config = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            // --version antes de qualquer coisa: o instalador (spec-wave
            // dev-agent --install) usa isso para decidir se precisa baixar o
            // binário.
            "--version" | "-V" => {
                println!("spec-wave-agent {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--config" | "-c" => {
                config = Some(it.next().ok_or_else(|| {
                    anyhow::anyhow!("--config exige um caminho")
                })?);
            }
            other => {
                // Falhar alto: um argumento ignorado silenciosamente vira um
                // daemon rodando contra o repo errado.
                bail!("argumento desconhecido: {other}\n\n{USAGE}");
            }
        }
    }
    Ok(Some(Args { config }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    init_tracing();
    let cfg = load_config_from(args.config.as_deref())?;
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

    // Issues processadas recentemente (qualquer desfecho) ficam em cooldown:
    // evita re-claim imediato por atraso do índice de busca do GitHub após
    // remover a label, e loop quente de retry quando uma feature falha.
    let mut cooldown: HashMap<u64, Instant> = HashMap::new();
    // Falhas SEGUIDAS por issue, com o instante da ÚLTIMA falha — item 2 do
    // rfc/plano-hardening-agentes-2026-08.md: sem o instante, 3 falhas
    // espaçadas por dias desistiam igual a 3 falhas seguidas em 10 minutos.
    // Zera no sucesso, ao tirar da fila, e ao expirar a janela (ver abaixo).
    // Em memória de propósito: o objetivo é parar o ciclo do daemon em
    // execução; depois de um restart, o humano já tem os comentários das
    // tentativas.
    let mut falhas: HashMap<u64, (u32, Instant)> = HashMap::new();
    // Issues das quais o agente DESISTIU (label removida por
    // `desistir_da_issue`) — item 2. Diferente do `cooldown`: uma issue aqui
    // só volta a ser processada quando uma leitura FRESCA (`gh issue view`,
    // sem o atraso do índice de busca que `gh issue list` consulta) confirma
    // que a label foi reaplicada. Sem isto, reaplicar a label não bastava —
    // só reiniciar o daemon destravava, porque nada verificava a label de
    // novo antes do próximo `cooldown_secs` natural expirar.
    let mut desistidas: HashMap<u64, Instant> = HashMap::new();
    // Rodadas de poll SEGUIDAS em que a fila voltou vazia — item 4. Zera na
    // primeira fila não-vazia; alimenta o backoff do sleep final do loop.
    let mut vazios_seguidos: u32 = 0;

    loop {
        if *sd_rx.borrow() {
            break;
        }
        let mut claimed = false;
        match queue::poll_queue(&cfg, &workdir).await {
            Ok(queue) => {
                // Item 4: só fila GENUINAMENTE vazia conta para o backoff.
                // Fila cheia sem claim (outro agente levou tudo) reseta
                // igual — não é ociosidade, é atividade que não é nossa.
                if queue.is_empty() {
                    vazios_seguidos += 1;
                } else {
                    vazios_seguidos = 0;
                }
                // Um item por vez: pega o PRIMEIRO que conseguir claimar (bugs
                // vêm antes de features — ver QueueKind);
                // ao terminar, volta direto ao poll (fila fresca).
                for item in queue {
                    let issue = item.number;
                    if *sd_rx.borrow() { break; }
                    if desistidas.contains_key(&issue) {
                        // Item 2: falha determinística. Só reprocessa se uma
                        // leitura FRESCA confirmar que a label foi reaplicada
                        // de verdade — `gh issue list` (poll_queue) pode
                        // devolver a issue por atraso do índice de busca
                        // mesmo já sem a label.
                        match queue::label_present_now(&cfg, &workdir, issue).await {
                            Ok(true) => { desistidas.remove(&issue); }
                            Ok(false) => continue,
                            Err(e) => {
                                warn!(target: "agent",
                                      "issue #{issue}: não deu para confirmar a label ({e:#}) — aguardando");
                                continue;
                            }
                        }
                    } else if let Some(t) = cooldown.get(&issue) {
                        if t.elapsed().as_secs() < cfg.cooldown_secs {
                            continue;
                        }
                    }
                    match process_issue(&cfg, &leases, &me, item,
                                        sd_rx.clone()).await {
                        Ok(a) if a.claimed => {
                            cooldown.insert(issue, Instant::now());
                            if a.failed {
                                // Item 2: janela do contador — falha antiga
                                // demais não conta na sequência atual. Sem
                                // isto, 3 falhas espaçadas por dias desistiam
                                // igual a 3 falhas seguidas em 10 minutos.
                                let janela = Duration::from_secs(
                                    cfg.cooldown_secs.saturating_mul(cfg.max_failures_per_issue as u64));
                                let entry = falhas.entry(issue).or_insert((0, Instant::now()));
                                if entry.1.elapsed() > janela {
                                    entry.0 = 0;
                                }
                                entry.0 += 1;
                                entry.1 = Instant::now();
                                let n = entry.0;
                                if n >= cfg.max_failures_per_issue {
                                    desistir_da_issue(&cfg, &workdir, &me, issue, n).await;
                                    falhas.remove(&issue);
                                    // Sai do cooldown comum: a partir de agora
                                    // é `desistidas` quem decide, com leitura
                                    // fresca — não os 15min cegos do cooldown.
                                    cooldown.remove(&issue);
                                    desistidas.insert(issue, Instant::now());
                                }
                            } else {
                                falhas.remove(&issue);
                            }
                            claimed = true;
                            break;
                        }
                        Ok(_) => {} // outro agente: tenta a próxima
                        Err(e) => {
                            error!(target: "agent", "erro na issue #{issue}: {e:#}");
                            cooldown.insert(issue, Instant::now());
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
        let delay = queue::next_poll_delay(
            vazios_seguidos.max(1), cfg.poll_interval_secs, cfg.poll_backoff_max_secs);
        tokio::select! {
            _ = sleep(TokioDuration::from_secs(delay)) => {}
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
