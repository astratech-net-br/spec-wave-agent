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
//!
//! Fonte de trabalho (`source` na config): `github-label` (default, a fila de
//! labels acima) ou `api` — os despachos da tela Development do spec-wave para
//! o login do dono do token de agente, em qualquer produto do tenant
//! (RFC-008 fase 2). O lease, o executor e o checkpoint são os mesmos.

use anyhow::{bail, Result};
use spec_wave_agent::api::{queue_item_of, ApiClient, Outcome, WorkItem};
use spec_wave_agent::config::{load_config_from, Config, Source};
use spec_wave_agent::lease::{Lease, LeaseRepo, RenewError};
use spec_wave_agent::queue::{self, QueueItem, QueueKind};
use spec_wave_agent::runner::{
    checkpoint_all, ensure_workspace, open_pull_requests_all, run_item_with_tap, RunEnd,
};
use spec_wave_agent::shell::run;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Heartbeat para a API do spec-wave (`source = "api"`), em paralelo ao do
/// lease. O primeiro sai na hora — é ele que tira o card de "Aguardando
/// agente". Se a API disser que o despacho não vale mais (cancelado, refeito
/// ou redirecionado), marca `cancelled` e dispara `stop`: o executor para, o
/// trabalho vai para o branch em checkpoint e o lease é liberado.
///
/// Falha de rede NÃO para o trabalho: a correção está no lease, a API é
/// informativa — o kanban só mostra "sem sinal" até o próximo heartbeat.
fn spawn_api_heartbeat(
    api: ApiClient, work: WorkItem, every_secs: u64,
    stop: Arc<watch::Sender<bool>>, cancelled: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match api.heartbeat(&work).await {
                Ok(reply) if !reply.keep_going => {
                    let why = reply.reason.unwrap_or_else(|| "o despacho mudou".into());
                    info!(target: "api", "#{}: parando — {why}", work.work_item);
                    cancelled.store(true, Ordering::SeqCst);
                    let _ = stop.send(true);
                    return;
                }
                Ok(_) => {}
                Err(e) => warn!(target: "api", "#{}: heartbeat falhou: {e:#}", work.work_item),
            }
            sleep(TokioDuration::from_secs(every_secs)).await;
        }
    })
}

/// Reporta o desfecho à API quando o item veio dela. Best-effort: o trabalho
/// já está pushado; um erro aqui vira aviso, não derruba o loop.
async fn report(api: Option<(&ApiClient, &WorkItem)>, outcome: Outcome) {
    if let Some((client, work)) = api {
        if let Err(e) = client.report(work, &outcome).await {
            warn!(target: "api", "#{}: não deu para reportar o desfecho: {e:#}", work.work_item);
        }
    }
}

/// Processa uma issue. `claimed` = este agente pegou a issue (e portanto a
/// fila deve ser re-consultada fresca); false = outro agente ficou com ela.
///
/// `api`: presente quando o item veio da API do spec-wave. Muda só a borda —
/// heartbeat e desfecho vão para a API em vez de labels no GitHub.
async fn process_issue(
    cfg: &Config, leases: &LeaseRepo, me: &str, item: QueueItem,
    shutdown: watch::Receiver<bool>,
    api: Option<(&ApiClient, &WorkItem)>,
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

    // `stop` junta o shutdown do processo e o cancelamento vindo da API: para o
    // executor os dois são a mesma coisa (checkpoint + release).
    let (stop_tx, stop_rx) = watch::channel(false);
    let stop_tx = Arc::new(stop_tx);
    let cancelled = Arc::new(AtomicBool::new(false));
    let forward = {
        let stop_tx = stop_tx.clone();
        let mut shutdown = shutdown;
        tokio::spawn(async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() { return; }
            }
            let _ = stop_tx.send(true);
        })
    };
    let api_hb = api.map(|(client, work)| spawn_api_heartbeat(
        client.clone(), work.clone(), cfg.heartbeat_secs, stop_tx.clone(), cancelled.clone()));

    // Transmissão ao vivo para a tela Development (fonte api): melhor esforço,
    // nunca bloqueia o executor (live.rs).
    // O stream de cada story (spec-wave implement ≥ 1.4) vai para um diretório
    // por execução, FORA do clone — não pode entrar em commit nenhum.
    let stream_dir = Path::new(&cfg.workdir).join("streams")
        .join(api.map(|(_, w)| w.run_id.clone()).unwrap_or_default());
    let live = api.and_then(|(client, work)| {
        std::fs::create_dir_all(&stream_dir).ok()?;
        client.start_live(work, stream_dir.clone())
    });
    let hooks = live.as_ref().map(|l| l.hooks());

    let outcome = async {
        let workspace = ensure_workspace(cfg, issue).await?;
        let end = run_item_with_tap(cfg, &workspace.hub, item, lost_rx.clone(), stop_rx, hooks).await?;
        Ok::<_, anyhow::Error>((workspace, end))
    }.await;
    if let Some(l) = live { l.finish().await; }
    // Mesmo sem transmissão (o Gateway não abriu), o diretório pode ter sido criado.
    if api.is_some() { let _ = std::fs::remove_dir_all(&stream_dir); }
    hb.abort();
    forward.abort();
    if let Some(h) = api_hb { h.abort(); }

    let (workspace, end) = match outcome {
        Ok(v) => v,
        Err(e) => {
            // Falha de infra (workspace/spawn): devolve o lease para não
            // prender a issue — mas só se ele ainda for nosso.
            if !*lost_rx.borrow() {
                let _ = leases.release(issue).await;
            }
            report(api, Outcome::Failed(format!("erro de infraestrutura no agente: {e:#}"))).await;
            return Err(e);
        }
    };
    let ws = &workspace.hub; // "gh issue ..." fala do hub — cwd não importa pra esses, qualquer um serviria

    let mut failed = false;
    match end {
        RunEnd::Success(result) => {
            // Cinto de segurança: garante push de qualquer resto que o
            // executor não tenha commitado, antes de sair da fila — no hub E
            // em cada repositório de código que a issue tocou.
            checkpoint_all(&workspace, issue, "success").await;
            // Num bug, o VALOR do trabalho está na causa raiz: sem publicá-la
            // na issue, ela morre no marker (que nem é commitado) e a próxima
            // pessoa reinvestiga o mesmo defeito.
            if item.kind == QueueKind::Bug {
                if let Some(body) = render_bug_report(&result, me) {
                    let _ = run(ws, "gh",
                        &["issue", "comment", &issue.to_string(),
                          "--repo", &cfg.repo, "--body", &body]).await;
                }
            }
            // O PR vem DEPOIS do checkpoint (todo o trabalho já está pushado) e
            // ANTES de sair da fila: é ele que dá conteúdo à etapa 👀 Code
            // Review. Sem isso, as Stories chegavam lá e a fila do Tech Leader
            // mostrava "sem PR" em todas. Um PR por repositório efetivamente
            // tocado (hub sempre; código de acordo com o que a issue declarou).
            let prs = open_pull_requests_all(&workspace, &cfg.repo, issue, cfg.pr_draft).await;
            if api.is_some() {
                // Pela API, "sair da fila" é o card ir para Review (com os PRs).
                report(api, Outcome::Succeeded(prs)).await;
            } else {
                let _ = run(ws, "gh",
                    &["issue", "edit", &issue.to_string(), "--repo", &cfg.repo,
                      "--remove-label", &cfg.queue_label]).await;
            }
            leases.release(issue).await?;
            info!(target: "agent", "{que} #{issue} concluído");
        }
        RunEnd::Failed(reason) => {
            checkpoint_all(&workspace, issue, "failed").await;
            let _ = run(ws, "gh",
                &["issue", "comment", &issue.to_string(), "--repo", &cfg.repo,
                  "--body",
                  &format!("⚠️ Agente `{me}` falhou: {reason}. Checkpoint \
                            pushado no branch — outro agente (ou humano) \
                            pode retomar; rode `npx spec-wave order {issue}` \
                            para ver o estado das stories. Item permanece \
                            na fila.")]).await;
            leases.release(issue).await?; // devolve p/ fila: label continua
            report(api, Outcome::Failed(reason.clone())).await;
            failed = true;
            warn!(target: "agent", "{que} #{issue} falhou: {reason}");
        }
        RunEnd::Interrupted => {
            // Dev desligando a máquina: checkpoint + release imediato para
            // takeover instantâneo (sem esperar TTL).
            checkpoint_all(&workspace, issue, "interrupted").await;
            leases.release(issue).await?;
            if cancelled.load(Ordering::SeqCst) {
                info!(target: "agent", "{que} #{issue}: despacho cancelado no spec-wave; \
                      checkpoint pushado e lease liberado");
            } else {
                info!(target: "agent", "issue #{issue} interrompida; lease liberado");
            }
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
async fn desistir_da_issue(
    cfg: &Config, cwd: &Path, me: &str, issue: u64, tentativas: u32,
    api: Option<(&ApiClient, &WorkItem)>,
) {
    if api.is_some() {
        // Pela API, desistir é o card ir para Blocked com o motivo — é lá que
        // uma pessoa decide (a tela tem "Devolver para Backlog"). Não há label.
        report(api, Outcome::Blocked(format!(
            "O agente `{me}` desistiu após {tentativas} falhas seguidas — \
             as tentativas estão nos comentários da issue."))).await;
        warn!(target: "agent", "issue #{issue}: {tentativas} falhas seguidas — bloqueada no spec-wave");
        return;
    }
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
(label `spec-wave:dev-agent`) ou, com `source = \"api\"`, os despachos da tela
Development do spec-wave — e delega a implementação ao Claude Code.

USO:
    spec-wave-agent [OPÇÕES]

OPÇÕES:
    -c, --config <PATH>  Arquivo de configuração TOML.
                         Default: $SPEC_WAVE_AGENT_CONFIG, ou
                         ~/.config/spec-wave-agent/config.toml
        --once           Executa UMA execução da frota (Fleet Job, RFC-008) a
                         partir do ambiente montado pelo spec-wave-sandbox
                         (SPECWAVE_HUB_REPO, SPECWAVE_WORK_ITEM, SPECWAVE_KIND,
                         RUN_ID, FLEET_WORKDIR) e emite eventos JSONL no
                         stdout. Config: --config, senão
                         $SPEC_WAVE_AGENT_CONFIG, senão os defaults.
    -V, --version        Imprime a versão e sai
    -h, --help           Imprime esta ajuda e sai

Sem opções, o agente roda em foreground até Ctrl+C (que faz checkpoint do
trabalho em andamento e libera o lease). Log via RUST_LOG (ex.: RUST_LOG=debug).";

/// Opções da linha de comando. Um parser à mão em vez de clap: são três
/// flags, e a dependência não se paga.
struct Args {
    config: Option<String>,
    once: bool,
}

/// `Ok(None)` = já respondeu (--help/--version) e o processo deve sair.
fn parse_args() -> Result<Option<Args>> {
    let mut config = None;
    let mut once = false;
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
            "--once" => once = true,
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
    Ok(Some(Args { config, once }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    init_tracing();
    if args.once {
        return run_once(args.config.as_deref()).await;
    }
    let cfg = load_config_from(args.config.as_deref())?;
    cfg.validate()?;
    let me = cfg.agent_id();
    std::fs::create_dir_all(&cfg.workdir)?;
    let workdir = Path::new(&cfg.workdir).to_path_buf();
    preflight(&workdir).await?;

    // Shutdown gracioso (SIGTERM do systemd/launchd, Ctrl+C)
    let (sd_tx, sd_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!(target: "agent", "sinal de shutdown recebido");
        let _ = sd_tx.send(true);
    });

    if cfg.source == Source::Api {
        return run_api_loop(&cfg, &me, sd_rx).await;
    }

    let leases = LeaseRepo::open_with_prefix(
        workdir.join("lease-repo"), &cfg.remote_url(), &cfg.lease_ref_prefix).await?;
    info!(target: "agent", "{me} iniciando; repo {} fila '{}'",
          cfg.repo, cfg.queue_label);

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
                                        sd_rx.clone(), None).await {
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
                                    desistir_da_issue(&cfg, &workdir, &me, issue, n, None).await;
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

/// Loop de `source = "api"`: os itens vêm da API do spec-wave (despachos da
/// tela Development para o login do dono do token), de QUALQUER produto do
/// tenant. Cada repositório ganha diretório de trabalho e lease próprios
/// (`Config::for_repo`); o resto — claim, executor, rodadas, checkpoint,
/// fencing — é exatamente o do modo por label.
///
/// Diferenças de política em relação ao modo por label:
/// - a chave de cooldown e de falhas é o `runId`: despachar de novo (ou
///   devolver ao WIP) gera outro runId e zera a contagem — decisão humana
///   explícita, como reaplicar a label;
/// - desistir é reportar Blocked (não há label para tirar), e o item some da
///   API por si só, sem a lista de `desistidas`.
async fn run_api_loop(cfg: &Config, me: &str, sd_rx: watch::Receiver<bool>) -> Result<()> {
    let client = ApiClient::from_config(cfg, me)?;
    let mut leases_by_repo: HashMap<String, LeaseRepo> = HashMap::new();
    let mut cooldown: HashMap<String, Instant> = HashMap::new();
    let mut falhas: HashMap<String, u32> = HashMap::new();
    let mut vazios_seguidos: u32 = 0;
    let mut anunciado = false;

    loop {
        if *sd_rx.borrow() {
            break;
        }
        let mut claimed = false;
        match client.fetch_work().await {
            Ok((login, items)) => {
                if !anunciado {
                    info!(target: "agent", "{me} iniciando; fonte api como `{login}`");
                    anunciado = true;
                }
                vazios_seguidos = if items.is_empty() { vazios_seguidos + 1 } else { 0 };
                for work in items {
                    if *sd_rx.borrow() { break; }
                    let Some(item) = queue_item_of(&work) else {
                        warn!(target: "agent", "{} #{}: tipo \"{}\" não suportado por esta versão — ignorado",
                              work.repo, work.work_item, work.kind);
                        continue;
                    };
                    if let Some(t) = cooldown.get(&work.run_id) {
                        if t.elapsed().as_secs() < cfg.cooldown_secs { continue; }
                    }
                    let item_cfg = match cfg.for_repo(&work.repo) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(target: "agent", "item #{} ignorado: {e:#}", work.work_item);
                            continue;
                        }
                    };
                    if !leases_by_repo.contains_key(&work.repo) {
                        // Falha aqui (repo sem acesso, rede) é de UM item: não
                        // pode derrubar o daemon — os outros itens seguem.
                        let opened = async {
                            std::fs::create_dir_all(&item_cfg.workdir)?;
                            LeaseRepo::open_with_prefix(
                                Path::new(&item_cfg.workdir).join("lease-repo"),
                                &item_cfg.remote_url(), &item_cfg.lease_ref_prefix).await
                        }.await;
                        match opened {
                            Ok(repo) => { leases_by_repo.insert(work.repo.clone(), repo); }
                            Err(e) => {
                                error!(target: "agent", "{} #{}: lease-repo indisponível: {e:#}",
                                       work.repo, work.work_item);
                                cooldown.insert(work.run_id.clone(), Instant::now());
                                continue;
                            }
                        }
                    }
                    let leases = &leases_by_repo[&work.repo];
                    let issue = item.number;
                    match process_issue(&item_cfg, leases, me, item, sd_rx.clone(),
                                        Some((&client, &work))).await {
                        Ok(a) if a.claimed => {
                            cooldown.insert(work.run_id.clone(), Instant::now());
                            if a.failed {
                                let n = falhas.entry(work.run_id.clone()).or_insert(0);
                                *n += 1;
                                if *n >= cfg.max_failures_per_issue {
                                    let n = *n;
                                    desistir_da_issue(&item_cfg, Path::new(&item_cfg.workdir), me,
                                                      issue, n, Some((&client, &work))).await;
                                    falhas.remove(&work.run_id);
                                }
                            } else {
                                falhas.remove(&work.run_id);
                            }
                            claimed = true;
                            break;
                        }
                        Ok(_) => {} // outro agente (outro host do mesmo dev): próxima
                        Err(e) => {
                            error!(target: "agent", "erro em {} #{issue}: {e:#}", work.repo);
                            cooldown.insert(work.run_id.clone(), Instant::now());
                            claimed = true;
                            break;
                        }
                    }
                }
            }
            Err(e) => error!(target: "agent", "consulta à API falhou: {e:#}"),
        }
        if claimed {
            continue;
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

// ---------- Modo --once: Fleet Job (RFC-008 fase 6, spec do sandbox §19.5) ----------
//
// O spec-wave-sandbox cria um Job por execução; dentro dele o fleet-runner
// lança `spec-wave-agent --once`. O agente faz exatamente o que faz no modo
// daemon para UM item — lease, executor, rodadas, checkpoint, PR —, mas:
//   - a execução vem do ambiente (não de fila nem de API);
//   - não fala com a API do spec-wave: quem reporta é o fleet-runner, pelo
//     sandbox. O agente só emite eventos JSONL no stdout (os logs vão para o
//     stderr, como sempre):
//       {"event":"claimed","generation":N}
//       {"event":"line","origin":null|"story:N","line":"<stream-json>"}
//       {"event":"outcome","state":"succeeded|failed|blocked|canceled|skipped",
//        "reason":"...","pr_urls":[...]}

/// Execução recebida do sandbox (env do Fleet Job).
struct FleetRun {
    hub: String,
    issue: u64,
    kind: QueueKind,
    run_id: String,
    workdir: String,
}

fn fleet_run_from_env() -> Result<FleetRun> {
    let var = |k: &str| std::env::var(k).map(|v| v.trim().to_string()).unwrap_or_default();
    let hub = var("SPECWAVE_HUB_REPO");
    let issue: u64 = var("SPECWAVE_WORK_ITEM").parse().ok().filter(|n| *n > 0)
        .ok_or_else(|| anyhow::anyhow!("SPECWAVE_WORK_ITEM ausente ou não é o número de uma issue"))?;
    let kind = match var("SPECWAVE_KIND").as_str() {
        "bug" => QueueKind::Bug,
        "feature" | "" => QueueKind::Feature,
        k => bail!("SPECWAVE_KIND desconhecido: {k}"),
    };
    let run_id = var("RUN_ID");
    let workdir = var("FLEET_WORKDIR");
    if hub.is_empty() || run_id.is_empty() || workdir.is_empty() {
        bail!("--once exige SPECWAVE_HUB_REPO, RUN_ID e FLEET_WORKDIR (o spec-wave-sandbox monta)");
    }
    Ok(FleetRun { hub, issue, kind, run_id, workdir })
}

/// Eventos para o fleet-runner: uma linha JSON por evento no stdout.
#[derive(Clone)]
struct Events(tokio::sync::mpsc::UnboundedSender<String>);

impl Events {
    fn start() -> (Events, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let task = tokio::spawn(async move {
            use std::io::Write;
            // Escrita síncrona: uma linha curta por evento, com flush — o
            // fleet-runner lê linha a linha e não pode ficar esperando buffer.
            while let Some(line) = rx.recv().await {
                let mut out = std::io::stdout().lock();
                if writeln!(out, "{line}").is_err() || out.flush().is_err() { return; }
            }
        });
        (Events(tx), task)
    }

    fn emit(&self, v: serde_json::Value) {
        let _ = self.0.send(v.to_string());
    }

    fn outcome(&self, state: &str, reason: Option<&str>, pr_urls: &[String]) {
        self.emit(serde_json::json!({
            "event": "outcome", "state": state, "reason": reason, "pr_urls": pr_urls,
        }));
    }
}

async fn run_once(config: Option<&str>) -> Result<()> {
    let (events, writer) = Events::start();
    let result = run_once_inner(config, &events).await;
    if let Err(e) = &result {
        // Erro de infraestrutura (clone, preflight, lease-repo): o fleet-runner
        // precisa de um desfecho mesmo assim.
        events.outcome("failed", Some(&format!("erro de infraestrutura no agente: {e:#}")), &[]);
    }
    drop(events);
    let _ = writer.await;
    result
}

async fn run_once_inner(config: Option<&str>, events: &Events) -> Result<()> {
    let job = fleet_run_from_env()?;
    // --config > SPEC_WAVE_AGENT_CONFIG (o cliente personaliza prompt e
    // executor da frota — ex.: um ConfigMap montado no Job) > defaults.
    let env_cfg = std::env::var("SPEC_WAVE_AGENT_CONFIG").ok().filter(|p| !p.trim().is_empty());
    let mut cfg = match config.map(str::to_string).or(env_cfg) {
        Some(path) => load_config_from(Some(&path))?,
        None => Config::defaults(),
    };
    cfg.source = Source::GithubLabel; // o item vem do ambiente; a borda é o fleet-runner
    cfg.repo = job.hub.clone();
    cfg.workdir = Path::new(&job.workdir).join("agent").to_string_lossy().into_owned();
    cfg.agent_id = Some(format!("fleet:{}", job.run_id));
    cfg.validate()?;
    let me = cfg.agent_id();
    std::fs::create_dir_all(&cfg.workdir)?;
    let workdir = Path::new(&cfg.workdir).to_path_buf();
    preflight(&workdir).await?;

    // SIGTERM (cancelamento pelo spec-wave, §19.9) = checkpoint + release.
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!(target: "agent", "sinal de término recebido — checkpoint e release");
        let _ = stop_tx.send(true);
    });

    let leases = LeaseRepo::open_with_prefix(
        workdir.join("lease-repo"), &cfg.remote_url(), &cfg.lease_ref_prefix).await?;
    let issue = job.issue;
    let Some(lease) = leases.try_acquire(issue, &me, cfg.lease_ttl_secs).await? else {
        events.outcome("skipped", Some("outro agente já detém o lease desta issue"), &[]);
        return Ok(());
    };
    events.emit(serde_json::json!({ "event": "claimed", "generation": lease.generation }));
    info!(target: "agent", "fleet {}: claim OK na issue #{issue} (gen {})", job.run_id, lease.generation);

    let (lost_tx, lost_rx) = watch::channel(false);
    let hb = spawn_heartbeat(leases.clone(), lease, cfg.heartbeat_secs, cfg.lease_ttl_secs, lost_tx);

    // Linhas do orquestrador (tap) e de cada story (arquivos que o
    // `spec-wave implement` ≥ 1.4 grava) viram eventos `line`.
    let stream_dir = Path::new(&job.workdir).join("streams").join(&job.run_id);
    std::fs::create_dir_all(&stream_dir)?;
    let (tap_tx, mut tap_rx) = tokio::sync::mpsc::channel::<spec_wave_agent::runner::LiveLine>(4096);
    let forward = {
        let events = events.clone();
        tokio::spawn(async move {
            while let Some(l) = tap_rx.recv().await {
                events.emit(serde_json::json!({ "event": "line", "origin": l.origin, "line": l.line }));
            }
        })
    };
    let stories = spec_wave_agent::live::StoryTail::spawn(stream_dir.clone(), tap_tx.clone());
    let hooks = spec_wave_agent::runner::LiveHooks { tap: tap_tx.clone(), stream_dir: stream_dir.clone() };
    drop(tap_tx);

    let item = QueueItem { kind: job.kind, number: issue };
    let outcome = async {
        let workspace = ensure_workspace(&cfg, issue).await?;
        let end = run_item_with_tap(&cfg, &workspace.hub, item, lost_rx.clone(), stop_rx, Some(hooks)).await?;
        Ok::<_, anyhow::Error>((workspace, end))
    }.await;
    stories.finish().await;
    let _ = forward.await;
    hb.abort();

    let (workspace, end) = match outcome {
        Ok(v) => v,
        Err(e) => {
            if !*lost_rx.borrow() {
                let _ = leases.release(issue).await;
            }
            return Err(e);
        }
    };
    match end {
        RunEnd::Success(result) => {
            checkpoint_all(&workspace, issue, "success").await;
            if item.kind == QueueKind::Bug {
                if let Some(body) = render_bug_report(&result, &me) {
                    let _ = run(&workspace.hub, "gh",
                        &["issue", "comment", &issue.to_string(), "--repo", &cfg.repo, "--body", &body]).await;
                }
            }
            let prs = open_pull_requests_all(&workspace, &cfg.repo, issue, cfg.pr_draft).await;
            leases.release(issue).await?;
            events.outcome("succeeded", None, &prs);
        }
        RunEnd::Failed(reason) => {
            checkpoint_all(&workspace, issue, "failed").await;
            leases.release(issue).await?;
            events.outcome("failed", Some(&reason), &[]);
        }
        RunEnd::Interrupted => {
            checkpoint_all(&workspace, issue, "interrupted").await;
            leases.release(issue).await?;
            events.outcome("canceled", Some("interrompido pelo spec-wave: checkpoint pushado no branch"), &[]);
        }
        RunEnd::LeaseLost => {
            // Sem checkpoint nem release: o novo dono manda.
            events.outcome("skipped", Some("lease perdido durante a execução — outro agente assumiu"), &[]);
        }
    }
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
