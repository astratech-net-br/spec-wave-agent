//! Workspace por issue + execução do `npx spec-wave implement` com
//! streaming do output do filho para o console do agente.

use crate::config::Config;
use crate::queue::{QueueItem, QueueKind};
use crate::shell::{retry_backoff, run, run_ok};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{timeout, Duration as TokioDuration};
use tracing::{info, warn};

/// Workspace completo de uma issue: o hub (sempre) e os repositórios de
/// código que ela declara (RFC-005 §6.3/§7.2 — "N clones" no lugar de um só).
pub struct Workspace {
    pub hub: PathBuf,
    pub code_repos: Vec<CodeRepoWorkspace>,
}

pub struct CodeRepoWorkspace {
    /// "owner/repo", para `--repo` do `gh`.
    pub repo: String,
    pub dir: PathBuf,
}

pub async fn ensure_workspace(cfg: &Config, issue: u64) -> Result<Workspace> {
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
    ensure_branch(&ws, issue).await?;
    exclude_scratch(&ws);

    // RFC-005 §6.3/§7.2: repositórios de código que a issue declara (labels
    // `repo:<slug>`; uma Feature agrega os das Stories). Reusa o `workspace
    // prepare` da CLI — a resolução slug→repo (via .spec-wave.json do hub) e
    // a convenção de clone já estão lá, testadas; reimplementar em Rust
    // duplicaria a mesma lógica em duas linguagens, com risco real de
    // divergir (achado da migração RFC-005 do lado spec-wave-ui/sandbox).
    let code_repos = prepare_code_repos(&ws, issue, &cfg.cli()).await?;

    Ok(Workspace { hub: ws, code_repos })
}

/// Arquivos de trabalho LOCAIS do spec-wave CLI que nunca podem ir para o
/// branch: o cache de rede e o contexto que o `implement` monta por item. O
/// checkpoint faz `git add -A` — num repositório cujo `.gitignore` não os
/// cobre, eles iam parar no PR (achado no MVP da frota, RFC-008 fase 7: +1.6k
/// linhas de contexto no PR de uma Feature de 34 linhas).
const SCRATCH_EXCLUDES: &[&str] = &[".spec-wave/cache/", ".spec-wave/implement-*.md", ".spec-wave-agent-qa.json"];

/// Acrescenta os padrões ao `.git/info/exclude` do clone — local, não mexe no
/// repositório de ninguém. Idempotente.
fn exclude_scratch(dir: &Path) {
    let path = dir.join(".git").join("info").join("exclude");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&str> = SCRATCH_EXCLUDES.iter().copied()
        .filter(|p| !current.lines().any(|l| l.trim() == *p))
        .collect();
    if missing.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# spec-wave-agent: arquivos de trabalho do spec-wave CLI\n");
    for p in missing {
        next.push_str(p);
        next.push('\n');
    }
    let _ = std::fs::write(&path, next);
}

/// Retomada: se o branch de trabalho já existe no remoto (outro agente
/// começou e caiu, ou é a segunda+ chamada desta função), continua dele;
/// senão cria a partir do default e empurra na hora — mesma lógica pro hub e
/// pra cada repositório de código (extraída daqui, ver `ensure_workspace` e
/// `prepare_code_repos`).
/// Clone do hub para a EXECUÇÃO DE QA (`QueueKind::Qa`): na branch principal,
/// em diretório próprio (`qa-<n>`). O `spec-wave qa` commita evidência e
/// `bug.md` e faz push na branch atual — na `agent/issue-<n>` do
/// desenvolvimento isso iria parar num PR (ou numa branch já mergeada).
pub async fn ensure_qa_workspace(cfg: &Config, issue: u64) -> Result<Workspace> {
    ensure_main_workspace(cfg, issue, true).await
}

/// Clone do hub na branch principal. `with_code_repos = false` é da etapa do
/// fluxo (`QueueKind::Flow`): o `spec-wave step` monta o workspace ele mesmo,
/// na branch principal, quando a etapa precisa — `prepare_code_repos` daqui
/// faria checkout (e PUSH) da `agent/issue-<n>` em cada repo de código, e o
/// modelo leria o WIP do desenvolvimento em vez do código mergeado.
pub async fn ensure_main_workspace(cfg: &Config, issue: u64, with_code_repos: bool) -> Result<Workspace> {
    let ws = PathBuf::from(&cfg.workdir).join(format!("qa-{issue}"));
    let url = cfg.remote_url();
    if !ws.join(".git").exists() {
        std::fs::create_dir_all(ws.parent().unwrap())?;
        retry_backoff("clone do workspace de QA", 3, Duration::from_secs(2), || {
            let url = url.clone();
            let ws = ws.clone();
            async move {
                run_ok(Path::new("."), "git",
                       &["clone", "--quiet", &url, ws.to_str().unwrap()]).await
            }
        }).await?;
    } else {
        retry_backoff("fetch do workspace de QA", 3, Duration::from_secs(2), || {
            let ws = ws.clone();
            async move { run_ok(&ws, "git", &["fetch", "--quiet", "origin"]).await }
        }).await?;
    }
    let head = run_ok(&ws, "git", &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"]).await
        .map(|o| o.trim().to_string())
        .unwrap_or_else(|_| "origin/main".into());
    let branch = head.strip_prefix("origin/").unwrap_or(&head).to_string();
    run_ok(&ws, "git", &["checkout", "--quiet", "-B", &branch, &head]).await?;
    run_ok(&ws, "git", &["reset", "--quiet", "--hard", &head]).await?;
    exclude_scratch(&ws);
    let code_repos = if with_code_repos {
        prepare_code_repos(&ws, issue, &cfg.cli()).await?
    } else {
        Vec::new()
    };
    Ok(Workspace { hub: ws, code_repos })
}

/// O hub define o executor de QA (`qa.command` no `.spec-wave.json`)?
fn hub_has_qa_command(ws: &Path) -> bool {
    std::fs::read_to_string(ws.join(".spec-wave.json")).ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("qa")?.get("command")?.as_str().map(|c| !c.trim().is_empty()))
        .unwrap_or(false)
}

/// Desfecho de uma execução de QA a partir do JSON do `--outcome-file`
/// (função PURA). O exit code do `spec-wave qa` não serve: reprova,
/// inconclusivo e falha do executor saem todos com 1.
///   pass / skipped / fail → Success "ok" (o QA terminou; na reprova a CLI já
///                           abriu os Bugs)
///   blocked / not-configured / refused → Success "blocked" (portão humano)
///   error / ausente → Failed (a frota tenta de novo)
fn qa_round_end(raw: Option<&str>, process: &str) -> RoundEnd {
    let Some(v) = raw.and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok()) else {
        return RoundEnd::Failed(format!("o spec-wave qa terminou sem desfecho ({process})"));
    };
    let outcome = v.get("outcome").and_then(|o| o.as_str()).unwrap_or("");
    let reason = v.get("reason").and_then(|r| r.as_str()).map(str::to_string);
    let nums = |key: &str| -> Vec<String> {
        v.get(key).and_then(|a| a.as_array()).map(|a| a.iter()
            .filter_map(|n| n.as_u64().map(|n| format!("#{n}")).or_else(|| n.as_str().map(|s| format!("#{s}"))))
            .collect()).unwrap_or_default()
    };
    let result = |status: &str, detalhe: String| RoundEnd::Success(Some(ExecResult {
        status: status.into(), detalhe: Some(detalhe), causa_raiz: None, fix: None,
        teste_regressao: None, arquivos: None, prs: None,
    }));
    match outcome {
        "pass" => {
            let pend = nums("pending");
            if !pend.is_empty() {
                result("blocked", format!("QA verde no que rodou, mas cenário(s) {} ainda pendente(s)", pend.join(", ")))
            } else if v.get("blockedByBugs").and_then(|b| b.as_bool()).unwrap_or(false) {
                result("ok", "QA verde; Bug aberto ainda segura a Story".into())
            } else {
                result("ok", "QA verde".into())
            }
        }
        "skipped" => result("ok", "nada a testar (item já passou de QA)".into()),
        "fail" => {
            let bugs = nums("bugs");
            result("ok", if bugs.is_empty() { "QA reprovou".into() }
                         else { format!("QA reprovou: Bug(s) {}", bugs.join(", ")) })
        }
        "blocked" => result("blocked", "QA inconclusivo: cenário(s) bloqueado(s), nada foi movido".into()),
        "not-configured" | "refused" =>
            result("blocked", reason.unwrap_or_else(|| format!("QA recusado ({outcome})"))),
        _ => RoundEnd::Failed(reason.unwrap_or_else(|| format!("QA terminou em erro ({outcome})"))),
    }
}

/// Desfecho de uma etapa do fluxo pelo exit code do `spec-wave step` (PURA):
/// 0 = concluída; 2 = recusa do fluxo (validação reprovada — a CLI já comentou
/// na issue, e a etapa TERMINOU); outro = falha (a CLI já fez o report-failure).
fn flow_round_end(raw: RoundRaw) -> RoundEnd {
    let ok = |detalhe: &str| RoundEnd::Success(Some(ExecResult {
        status: "ok".into(), detalhe: Some(detalhe.into()), causa_raiz: None, fix: None,
        teste_regressao: None, arquivos: None, prs: None,
    }));
    match raw {
        RoundRaw::Success => ok("etapa concluída"),
        RoundRaw::Failed(r) if r == "exit code 2" =>
            ok("etapa concluída com recusa do fluxo — o motivo está comentado na issue"),
        RoundRaw::Failed(r) => RoundEnd::Failed(r),
        RoundRaw::LeaseLost => RoundEnd::LeaseLost,
        RoundRaw::Interrupted => RoundEnd::Interrupted,
    }
}

async fn ensure_branch(dir: &Path, issue: u64) -> Result<()> {
    let branch = work_branch(issue);
    let remote = run(dir, "git",
                     &["ls-remote", "--exit-code", "origin",
                       &format!("refs/heads/{branch}")]).await?;
    if remote.ok {
        run_ok(dir, "git", &["checkout", "-B", &branch,
                             &format!("origin/{branch}")]).await?;
        info!(target: "ws", "retomando branch existente {branch} em {}", dir.display());
    } else {
        let head = run_ok(dir, "git",
            &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"]).await
            .unwrap_or_else(|_| "origin/main".into());
        run_ok(dir, "git", &["checkout", "-B", &branch, head.trim()]).await?;
        run_ok(dir, "git", &["push", "--quiet", "-u", "origin", &branch]).await?;
    }
    Ok(())
}

/// Saída de `spec-wave workspace prepare --issue <n> --json` que interessa
/// aqui — o resto (`workspaceDir`, `cloned`, `missing`...) é só para quem
/// pede legibilidade humana.
#[derive(Debug, serde::Deserialize)]
struct WorkspacePrepareOutput {
    repos: Vec<WorkspacePrepareRepo>,
}

#[derive(Debug, serde::Deserialize)]
struct WorkspacePrepareRepo {
    #[allow(dead_code)]
    slug: String,
    #[allow(dead_code)]
    owner: String,
    repo: String,
    dir: String,
}

/// Chama a CLI pra resolver e clonar os repositórios de código da issue, e
/// garante o branch de trabalho em cada um (a CLI clona no default branch —
/// quem commita ali por cima de uma sessão do agente é este passo).
///
/// Sem `.spec-wave.json` no hub, nem tenta: é um hub anterior à RFC-005 (ou
/// um fixture de teste sem contexto de CLI nenhum), e mono-repo é o caso
/// degenerado por desenho — `spec-wave workspace prepare` recusaria de
/// qualquer jeito ("rode dentro do hub"). Com `.spec-wave.json` presente, uma
/// falha de verdade (rede, repo declarado inacessível) SOBE — silenciar ali
/// esconderia a issue chegando com código faltando, sem aviso nenhum.
async fn prepare_code_repos(hub: &Path, issue: u64, cli: &str) -> Result<Vec<CodeRepoWorkspace>> {
    if !hub.join(".spec-wave.json").exists() {
        return Ok(Vec::new());
    }
    // `cli` = como chamar a CLI (config::detect_cli) — nunca o `npx` cru: o
    // hub pode ter uma cópia antiga dela no próprio node_modules.
    let mut argv = crate::config::split_command(cli)?;
    if argv.is_empty() {
        anyhow::bail!("cli_command vazio");
    }
    let issue_arg = issue.to_string();
    argv.extend(["workspace", "prepare", "--issue", &issue_arg, "--json"].map(String::from));
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let out = run_ok(hub, &argv[0], &args)
        .await
        .context("`spec-wave workspace prepare` falhou")?;
    let parsed: WorkspacePrepareOutput = serde_json::from_str(out.trim())
        .with_context(|| format!("`spec-wave workspace prepare --json`: saída inesperada: {out:?}"))?;
    let mut code_repos = Vec::with_capacity(parsed.repos.len());
    for r in parsed.repos {
        let dir = PathBuf::from(&r.dir);
        ensure_branch(&dir, issue).await
            .with_context(|| format!("branch de trabalho em {} ({})", r.repo, dir.display()))?;
        exclude_scratch(&dir);
        code_repos.push(CodeRepoWorkspace { repo: r.repo, dir });
    }
    Ok(code_repos)
}

/// Há commits no branch de trabalho que não estão no branch default deste
/// repositório — só então vale abrir PR ou fazer checkpoint de verdade. Sem
/// isto, TODO repositório de código vinculado ganharia um PR vazio a cada
/// issue, mesmo o que a Feature nunca tocou (RFC-005 §6.3: "N PRs" não é
/// "sempre N", é "um por repositório efetivamente tocado").
pub async fn has_unmerged_commits(dir: &Path) -> bool {
    let default = run_ok(dir, "git",
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"]).await
        .unwrap_or_else(|_| "origin/main".into());
    match run(dir, "git", &["rev-list", "--count", &format!("{}..HEAD", default.trim())]).await {
        Ok(o) if o.ok => o.stdout.trim().parse::<u64>().unwrap_or(0) > 0,
        _ => false,
    }
}

/// Nome do branch de trabalho de uma issue.
pub fn work_branch(issue: u64) -> String {
    format!("agent/issue-{issue}")
}

/// Corpo do PR de trabalho.
///
/// A primeira linha é o VÍNCULO que a CLI lê (`spec-wave code-review` / `qa`,
/// lib/pr-link.mjs): verbo `Implements` + `owner/repo#N`. O verbo em português
/// ("Implementa a issue …") não casa com a régua de lá, e o merge do PR deixava
/// a Feature parada no board ("PR sem vínculo explícito"). `Implements` não é
/// palavra-chave de fechamento do GitHub — o merge segue sem encerrar a issue.
fn pr_body(hub_repo: &str, issue: u64) -> String {
    format!(
        "Implements {hub_repo}#{issue}.\n\n\
         Branch de trabalho do `spec-wave-agent`, com um commit por Story.\n\n\
         O merge **não** encerra a issue: ela segue por QA, Homologação e Deploy \
         no board."
    )
}

/// Abre o Pull Request do trabalho concluído, se ainda não houver um.
///
/// UM PR por issue, não por story: o agente trabalha num branch único
/// (`agent/issue-N`) para a feature inteira, então "um PR por Story" não teria
/// como existir. Sem este passo, as Stories chegavam a 👀 Code Review e a fila
/// do Tech Leader mostrava "sem PR" em todas — uma etapa de revisão sem nada
/// para revisar.
///
/// Best-effort de propósito: o trabalho já está commitado e pushado, e falhar a
/// execução inteira por causa do PR perderia o lease sem motivo. Um aviso basta
/// para o humano abrir à mão.
///
/// Sem palavra-chave de fechamento (`Closes`/`Fixes`) no corpo: o merge não
/// encerra o item — ele ainda percorre QA, Homologação e Deploy no board.
///
/// `pr_repo` é onde o PR nasce; `hub_repo` é onde a issue #{issue} vive — nos
/// repositórios de código de uma sessão multi-repo (RFC-005 §6.3) os dois
/// divergem, e a referência no corpo/título precisa da forma qualificada
/// (`owner/repo#N`), porque `#{issue}` sozinho num repo de código resolveria
/// (se resolvesse) para uma issue LOCAL daquele repo, não a do hub.
///
/// Devolve a URL do PR (novo ou o que já existia) — o desfecho leva ao
/// spec-wave, e o card em Review mostra o link.
pub async fn open_pull_request(
    ws: &Path, pr_command: &[String], pr_repo: &str, hub_repo: &str, issue: u64, draft: bool,
) -> Option<String> {
    let branch = work_branch(issue);
    let body = pr_body(hub_repo, issue);
    match open_pull_request_via_cli(ws, pr_command, pr_repo, hub_repo, issue, &branch, &body, draft).await {
        CliPr::Opened(url) => {
            info!(target: "agent", "PR em {pr_repo}: {url}");
            return Some(url);
        }
        CliPr::NothingToReview => {
            warn!(target: "agent", "{branch} não tem nada à frente da base em {pr_repo} — nenhum PR aberto");
            return None;
        }
        CliPr::Unavailable(why) => {
            warn!(target: "agent", "PR pela CLI indisponível ({why}) — tentando o gh");
        }
    }
    open_pull_request_gh(ws, pr_repo, hub_repo, issue, &branch, &body, draft).await
}

/// Resultado do `spec-wave pr open`.
#[derive(Debug, PartialEq)]
pub enum CliPr {
    Opened(String),
    /// Código 2: a branch não tem nada à frente da base.
    NothingToReview,
    /// A CLI não abriu (comando ausente numa CLI antiga, credencial, rede):
    /// quem chama tenta o caminho de antes.
    Unavailable(String),
}

/// Interpreta a saída do `spec-wave pr open --json`. Pura, para os testes.
pub fn parse_cli_pr(code: Option<i32>, stdout: &str, stderr: &str) -> CliPr {
    if code == Some(0) {
        let url = stdout.lines().rev()
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_string))
            .filter(|u| !u.is_empty());
        return match url {
            Some(u) => CliPr::Opened(u),
            None => CliPr::Unavailable(format!("saída sem url: {}", stdout.trim())),
        };
    }
    if code == Some(2) {
        return CliPr::NothingToReview;
    }
    let why = stderr.trim().lines().last().unwrap_or("").to_string();
    CliPr::Unavailable(if why.is_empty() { format!("código {code:?}") } else { why })
}

/// Abre o PR pela porta de SCM da CLI (RFC-008 P17): idempotente como o
/// caminho do gh — um PR já aberto da branch é devolvido, não duplicado. O
/// título vem do item no tracker (`--work-item` + `--hub`).
#[allow(clippy::too_many_arguments)]
async fn open_pull_request_via_cli(
    ws: &Path, pr_command: &[String], pr_repo: &str, hub_repo: &str, issue: u64, branch: &str,
    body: &str, draft: bool,
) -> CliPr {
    let Some((program, base)) = pr_command.split_first() else {
        return CliPr::Unavailable("pr_command vazio".into());
    };
    let issue_s = issue.to_string();
    let mut args: Vec<&str> = base.iter().map(String::as_str).collect();
    args.extend(["--repo", pr_repo, "--head", branch, "--work-item", &issue_s, "--hub", hub_repo,
                 "--body", body, "--json"]);
    if draft {
        args.push("--draft");
    }
    match run(ws, program, &args).await {
        Ok(o) => parse_cli_pr(o.code, &o.stdout, &o.stderr),
        Err(e) => CliPr::Unavailable(format!("{e:#}")),
    }
}

/// O caminho de antes da CLI: `gh pr list` + `gh pr create`. Fica como
/// reserva enquanto houver CLI anterior à 1.7.0 no caminho (máquinas de dev,
/// imagem do Fleet Job) — e só serve ao GitHub.
async fn open_pull_request_gh(
    ws: &Path, pr_repo: &str, hub_repo: &str, issue: u64, branch: &str, body: &str, draft: bool,
) -> Option<String> {
    // Idempotência: relançamento após rodada incompleta, ou takeover por outro
    // agente, não podem gerar um segundo PR do mesmo branch.
    if let Ok(existing) = run(ws, "gh", &["pr", "list", "--repo", pr_repo, "--head", branch,
                                          "--state", "open", "--json", "url",
                                          "--jq", ".[].url"]).await {
        let url = existing.stdout.trim().lines().next().unwrap_or("").to_string();
        if existing.ok && !url.is_empty() {
            info!(target: "agent", "PR do #{issue} já existe em {pr_repo} ({url}) — nada a fazer");
            return Some(url);
        }
    }
    // Título da issue: um PR chamado "agent/issue-18" não diz nada em uma fila
    // de revisão. Falha de rede aqui não impede o PR — cai no genérico.
    let title = match run(ws, "gh", &["issue", "view", &issue.to_string(),
                                      "--repo", hub_repo, "--json", "title",
                                      "--jq", ".title"]).await {
        Ok(o) if o.ok && !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        _ => format!("Implementação da issue {hub_repo}#{issue}"),
    };
    let mut args = vec!["pr", "create", "--repo", pr_repo, "--head", branch,
                        "--title", &title, "--body", body];
    if draft {
        args.push("--draft");
    }
    match run(ws, "gh", &args).await {
        Ok(o) if o.ok => {
            let url = o.stdout.trim().lines().last().unwrap_or("").to_string();
            info!(target: "agent", "PR aberto em {pr_repo}: {url}");
            (!url.is_empty()).then_some(url)
        }
        Ok(o) => {
            warn!(target: "agent",
                "não foi possível abrir o PR do #{issue} em {pr_repo} (abra à mão a partir de \
                 {branch}): {}", o.stderr.trim());
            None
        }
        Err(e) => {
            warn!(target: "agent",
                "não foi possível abrir o PR do #{issue} em {pr_repo} (abra à mão a partir de \
                 {branch}): {e:#}");
            None
        }
    }
}

/// Checkpoint do hub e de cada repositório de código — mesmo cinto de
/// segurança do hub (garante push de qualquer resto não commitado),
/// estendido pra todo repositório que a issue pode ter tocado.
pub async fn checkpoint_all(workspace: &Workspace, issue: u64, label: &str) {
    checkpoint(&workspace.hub, issue, label).await;
    for cr in &workspace.code_repos {
        checkpoint(&cr.dir, issue, label).await;
    }
}

/// Abre o PR do hub (sempre, como já era) e, de cada repositório de código,
/// só o que tem commit de verdade no branch de trabalho (RFC-005 §6.3:
/// "N PRs" é um por repositório EFETIVAMENTE tocado, não todo repositório
/// vinculado ao Project).
pub async fn open_pull_requests_all(
    workspace: &Workspace, pr_command: &[String], hub_repo: &str, issue: u64, draft: bool,
) -> Vec<String> {
    open_pull_requests_with_dirs(workspace, pr_command, hub_repo, issue, draft).await
        .into_iter().map(|(url, _)| url).collect()
}

/// `open_pull_requests_all` devolvendo também o clone de cada PR — o gate de
/// CI compara os checks com o HEAD daquele clone.
async fn open_pull_requests_with_dirs(
    workspace: &Workspace, pr_command: &[String], hub_repo: &str, issue: u64, draft: bool,
) -> Vec<(String, PathBuf)> {
    let mut prs = Vec::new();
    if let Some(url) = open_pull_request(&workspace.hub, pr_command, hub_repo, hub_repo, issue, draft).await {
        prs.push((url, workspace.hub.clone()));
    }
    for cr in &workspace.code_repos {
        if has_unmerged_commits(&cr.dir).await {
            if let Some(url) = open_pull_request(&cr.dir, pr_command, &cr.repo, hub_repo, issue, draft).await {
                prs.push((url, cr.dir.clone()));
            }
        }
    }
    prs
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

#[derive(Debug)]
pub enum RunEnd {
    Success(Option<ExecResult>),
    Failed(String),
    /// O `spec-wave implement` recusou começar uma Story cuja dependência ainda
    /// não tem código na base (registro `blocked.json`). NÃO é falha: o
    /// trabalho espera o merge de outra Feature e retoma depois.
    Blocked(BlockedRecord),
    /// O executor parou DE PROPÓSITO (marker `{"status": "blocked"}`): achou
    /// um impedimento que só uma pessoa resolve — a spec manda parar e
    /// reportar, falta um insumo fora do repositório. Não é falha (repetir dá
    /// o mesmo resultado e gasta uma execução inteira) nem espera por
    /// dependência (nada destrava sozinho): vai direto para o portão humano.
    Stopped(String),
    LeaseLost,
    Interrupted,
}

/// Registro que o `spec-wave implement` (≥ 1.15) grava em
/// `<git-dir>/spec-wave/blocked.json` quando uma Story espera dependência.
///
/// Fica dentro do `.git` (fora da árvore): um `git add -A` do executor não o
/// commita, e o agente o lê sem depender do que o modelo escreveu no marker.
pub const BLOCKED_RECORD_REL: &str = "spec-wave/blocked.json";
/// Única versão do formato que este agente entende.
pub const BLOCKED_RECORD_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedStory {
    pub story: u64,
    #[serde(default)]
    pub waiting_on: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedRecord {
    pub version: u64,
    #[serde(default)]
    pub issue: Option<u64>,
    /// Hoje só `waiting_dependency`.
    pub reason: String,
    #[serde(default)]
    pub waiting_on: Vec<u64>,
    #[serde(default)]
    pub blocked: Vec<BlockedStory>,
    /// Quantas Stories ainda puderam rodar antes do corte.
    #[serde(default)]
    pub runnable: u64,
}

impl BlockedRecord {
    /// Motivo legível para o desfecho (issue, tela, log).
    pub fn describe(&self) -> String {
        let deps = |v: &[u64]| v.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
        let stories: Vec<String> = self.blocked.iter()
            .map(|b| format!("Story #{} aguarda {}", b.story, deps(&b.waiting_on)))
            .collect();
        let base = if stories.is_empty() {
            format!("aguardando {}", deps(&self.waiting_on))
        } else {
            stories.join("; ")
        };
        format!("bloqueada por dependência: {base} — o código ainda não está na base")
    }
}

/// Interpreta o conteúdo de `blocked.json` (função PURA). Versão desconhecida
/// ou JSON inválido = `None`: um registro que o agente não entende não pode
/// virar bloqueio (o run seguiria preso sem motivo verificável).
pub fn parse_blocked_record(raw: &str) -> Option<BlockedRecord> {
    let r: BlockedRecord = serde_json::from_str(raw.trim()).ok()?;
    (r.version == BLOCKED_RECORD_VERSION && !r.reason.trim().is_empty()).then_some(r)
}

/// Caminho do registro no clone (respeita worktree, onde `.git` é arquivo).
async fn blocked_record_path(ws: &Path) -> Option<PathBuf> {
    let git_dir = run(ws, "git", &["rev-parse", "--absolute-git-dir"]).await.ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(ws.join(".git")).filter(|d| d.is_dir()))?;
    Some(git_dir.join(BLOCKED_RECORD_REL))
}

/// Lê e REMOVE o registro de bloqueio (um registro velho faria a próxima
/// rodada reportar um bloqueio que já destravou).
async fn take_blocked_record(ws: &Path) -> Option<BlockedRecord> {
    let path = blocked_record_path(ws).await?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let rec = parse_blocked_record(&raw);
    if rec.is_none() {
        warn!(target: "agent", "registro de bloqueio ignorado: formato desconhecido em {}", path.display());
    }
    rec
}

/// Marker que o executor escreve ao final (ver feature_prompt): exit 0 sem
/// marker "ok" NÃO é sucesso — pega orquestrador que encerrou prematuramente
/// (ex: deixou implements em background).
const RESULT_MARKER: &str = ".spec-wave-agent-result.json";

/// Marker de conclusão do executor.
///
/// Todos os campos além de `status` são `#[serde(default)]`: um executor antigo
/// (ou um prompt customizado pelo usuário) escreve só `{"status": "ok"}` e
/// continua válido. Os campos de RCA são preenchidos pelo prompt de BUG e
/// viram o comentário na issue ao concluir.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExecResult {
    pub status: String,
    #[serde(default)]
    pub detalhe: Option<String>,
    #[serde(default)]
    pub causa_raiz: Option<String>,
    #[serde(default)]
    pub fix: Option<String>,
    #[serde(default)]
    pub teste_regressao: Option<String>,
    #[serde(default)]
    pub arquivos: Option<Vec<String>>,
    /// Revisão de PR: URL de cada PR revisado. Vazio = nenhum PR elegível.
    #[serde(default)]
    pub prs: Option<Vec<String>>,
}

/// Lê e REMOVE o marker (não pode sobrar para o checkpoint commitar).
fn take_result_marker(ws: &Path) -> Option<ExecResult> {
    let path = ws.join(RESULT_MARKER);
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(raw.trim()).ok()
}

/// Truncagem segura em fronteira de char, com reticências.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Resume um evento do stream-json do Claude Code (`--output-format
/// stream-json`) em uma linha legível. None = evento sem interesse (ex:
/// tool_result), que é suprimido do console.
fn format_stream_event(v: &serde_json::Value) -> Option<String> {
    match v.get("type").and_then(|t| t.as_str())? {
        "system" => {
            // Só o init interessa; outros subtypes (status etc.) são ruído.
            if v.get("subtype").and_then(|s| s.as_str()) != Some("init") {
                return None;
            }
            let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
            Some(format!("sessão iniciada (modelo {model})"))
        }
        "assistant" => {
            let content = v.pointer("/message/content")?.as_array()?;
            let mut parts = Vec::new();
            for c in content {
                match c.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        let t = c.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !t.trim().is_empty() {
                            parts.push(trunc(t.trim(), 400));
                        }
                    }
                    Some("tool_use") => {
                        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let detail = c.get("input").and_then(|i| {
                            ["command", "file_path", "description", "prompt", "pattern"]
                                .iter()
                                .find_map(|k| i.get(k).and_then(|s| s.as_str()))
                        }).unwrap_or("");
                        parts.push(format!("⏵ {name}: {}", trunc(detail, 160)));
                    }
                    _ => {}
                }
            }
            (!parts.is_empty()).then(|| parts.join(" | "))
        }
        "result" => {
            let sub = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("?");
            let cost = v.get("total_cost_usd").and_then(|c| c.as_f64())
                .map(|c| format!(", custo US${c:.2}")).unwrap_or_default();
            let turns = v.get("num_turns").and_then(|t| t.as_u64())
                .map(|t| format!(", {t} turnos")).unwrap_or_default();
            Some(format!("fim: {sub}{cost}{turns}"))
        }
        _ => None, // "user" (tool results) e afins: ruído
    }
}

/// Linha do stdout do executor: stream-json vira resumo legível; linha que
/// não é stream-json passa crua (executor sem --output-format stream-json).
fn render_out_line(line: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) if v.get("type").is_some() => format_stream_event(&v),
        _ => Some(line.to_string()),
    }
}

/// Uma linha de saída para a transmissão ao vivo. `origin`: None = o
/// orquestrador (stdout do executor); `Some("story:<n>")` = o agente interno de
/// uma story, lido do arquivo que o `spec-wave implement` grava (P24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLine {
    pub origin: Option<String>,
    pub line: String,
}

/// Canal opcional que recebe cada linha crua do stdout do executor — a
/// transmissão ao vivo (live.rs) lê daqui. `try_send`: cheio, descarta; o
/// executor nunca espera por quem assiste.
pub type LineTap = tokio::sync::mpsc::Sender<LiveLine>;

/// Ganchos da transmissão ao vivo para o executor: o canal das linhas e o
/// diretório onde o `spec-wave implement` grava o stream de cada story
/// (vira SPEC_WAVE_STREAM_DIR no ambiente do executor).
#[derive(Debug, Clone)]
pub struct LiveHooks {
    pub tap: LineTap,
    pub stream_dir: PathBuf,
    /// Rodada corrente do executor (1, 2, …) — quem acompanha o progresso
    /// (progress.rs) lê daqui.
    pub round: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

async fn stream_lines<R>(reader: R, issue: u64, stream: &'static str, tap: Option<LineTap>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(t) = &tap {
            let _ = t.try_send(LiveLine { origin: None, line: line.clone() });
        }
        if stream == "out" {
            if let Some(msg) = render_out_line(&line) {
                info!(target: "spec-wave", "[#{issue}] {msg}");
            }
        } else {
            info!(target: "spec-wave", "[#{issue}][{stream}] {line}");
        }
    }
}

/// Job object do Windows: todo processo criado por um processo do job entra
/// no mesmo job, então TerminateJobObject mata a árvore inteira — é o
/// equivalente do SIGKILL no grupo de processos do Unix.
#[cfg(windows)]
mod job {
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::ptr::null;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        TerminateJobObject, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct JobHandle(HANDLE);

    // SAFETY: HANDLE é um ponteiro opaco do kernel; só usamos
    // TerminateJobObject/CloseHandle, ambos thread-safe.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// Cria o job com KILL_ON_JOB_CLOSE e anexa o processo.
        pub fn assign(pid: u32) -> Result<Self> {
            unsafe {
                let job = CreateJobObjectW(null(), null());
                if job.is_null() {
                    bail!("CreateJobObjectW falhou");
                }
                // KILL_ON_JOB_CLOSE: fechar o handle mata o que sobrou —
                // mesma semântica do kill_on_drop do tokio, porém na árvore.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    CloseHandle(job);
                    bail!("SetInformationJobObject falhou");
                }
                let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if proc.is_null() {
                    CloseHandle(job);
                    bail!("OpenProcess falhou para o pid {pid}");
                }
                let assigned = AssignProcessToJobObject(job, proc);
                CloseHandle(proc);
                if assigned == 0 {
                    CloseHandle(job);
                    bail!("AssignProcessToJobObject falhou");
                }
                Ok(Self(job))
            }
        }

        pub fn terminate(&self) {
            unsafe { TerminateJobObject(self.0, 1); }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: fechar o handle encerra o que restar.
            unsafe { CloseHandle(self.0); }
        }
    }
}

/// Mecanismo de kill de árvore, por plataforma. O executor spawna
/// `npx spec-wave`, que spawna o claude interno: matar só o filho direto
/// deixaria netos órfãos trabalhando — buraco de fencing.
struct ProcessTree {
    /// Unix: o filho é líder do grupo (process_group(0) no spawn).
    #[cfg(unix)]
    pgid: Option<u32>,
    /// Windows: job object ao qual o filho e todos os netos pertencem.
    #[cfg(windows)]
    job: Option<job::JobHandle>,
}

impl ProcessTree {
    /// Anexa o filho recém-spawnado ao mecanismo da plataforma.
    fn attach(child: &tokio::process::Child) -> Self {
        #[cfg(unix)]
        {
            Self { pgid: child.id() }
        }
        #[cfg(windows)]
        {
            let job = child.id().and_then(|pid| match job::JobHandle::assign(pid) {
                Ok(j) => Some(j),
                Err(e) => {
                    // Sem job object o fencing cobre só o filho direto —
                    // degrada, mas avisa alto.
                    tracing::warn!(target: "agent",
                        "job object indisponível ({e:#}); netos podem sobreviver ao kill");
                    None
                }
            });
            Self { job }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {}
        }
    }

    /// Mata a árvore inteira (best-effort; idempotente).
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pgid {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

async fn kill_tree(child: &mut tokio::process::Child, tree: &ProcessTree) {
    tree.kill();
    let _ = child.kill().await; // reap (e fallback sem árvore)
}

/// Desfecho bruto do processo do executor (antes da checagem do marker).
enum RoundRaw { Success, Failed(String), LeaseLost, Interrupted }

/// Desfecho de UMA rodada do executor, já com o marker aplicado.
enum RoundEnd {
    Success(Option<ExecResult>), Incomplete, Failed(String), Blocked(BlockedRecord),
    Stopped(String), LeaseLost, Interrupted,
}

/// Roda UMA rodada do executor da feature (Claude Code orquestrador): o
/// comando vem de `cfg.feature_command` e o prompt entra via STDIN.
async fn executor_round(
    cfg: &Config, ws: &Path, item: QueueItem,
    mut lease_lost: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
    live: Option<LiveHooks>,
    extra_prompt: Option<&str>,
) -> Result<RoundEnd> {
    let issue = item.number;
    let (command, prompt_tpl, timeout_secs) = match item.kind {
        QueueKind::Bug => (&cfg.bug_command, &cfg.bug_prompt, cfg.bug_timeout_secs),
        QueueKind::Feature =>
            (&cfg.feature_command, &cfg.feature_prompt, cfg.implement_timeout_secs),
        QueueKind::Review =>
            (&cfg.review_command, &cfg.review_prompt, cfg.review_timeout_secs),
        // QA: a CLI conduz tudo (o executor dos cenários é chamado por ela);
        // nada entra no stdin.
        QueueKind::Qa => (&cfg.qa_command, &cfg.qa_command, cfg.qa_timeout_secs),
        // Etapa do fluxo: a CLI roda o corpo do job (`spec-wave step`); nada no stdin.
        QueueKind::Flow => (&cfg.flow_command, &cfg.flow_command, cfg.flow_timeout_secs),
    };
    // `{step}` antes do render: é texto da config, não da issue.
    let command = &command.replace("{step}", cfg.flow_step.as_deref().unwrap_or(""));
    // Frota: MCP configurável (SPECWAVE_MCP_CONFIG / SPECWAVE_MCP_ALLOWED_TOOLS).
    // Sem as envs o argv fica idêntico; nada do conteúdo é logado.
    let argv = crate::config::apply_mcp_env(crate::config::split_command(
        &cfg.render(command, issue))?);
    // O extra (contexto de CI reprovado) entra DEPOIS do render: log de CI é
    // texto alheio, e um "{issue}" dentro dele não é placeholder.
    let mut argv = argv;
    if item.kind == QueueKind::Qa {
        // Reteste de cenários específicos (ciclo de Bugs): o sandbox manda a
        // lista em SPECWAVE_QA_ONLY.
        if let Some(only) = std::env::var("SPECWAVE_QA_ONLY").ok().filter(|o| !o.trim().is_empty()) {
            argv.extend(["--only".to_string(), only.trim().to_string()]);
        }
        let _ = std::fs::remove_file(ws.join(crate::config::QA_OUTCOME_FILE));
    }
    // Feature/Bug: o registro de bloqueio desta rodada é o único que vale.
    let reads_blocked = matches!(item.kind, QueueKind::Feature | QueueKind::Bug);
    if reads_blocked {
        let _ = take_blocked_record(ws).await;
    }
    let mut prompt = if matches!(item.kind, QueueKind::Qa | QueueKind::Flow) {
        String::new()
    } else {
        cfg.render(prompt_tpl, issue)
    };
    if let Some(extra) = extra_prompt {
        prompt.push_str(extra);
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // O `spec-wave implement` (≥ 1.4) grava ali o stream de cada story.
    if let Some(h) = &live {
        cmd.env("SPEC_WAVE_STREAM_DIR", &h.stream_dir);
    }
    // A CLI que conduz o fluxo (config::detect_cli): o `spec-wave implement`
    // a repete nas instruções do agente interno, que também roda no clone.
    cmd.env("SPEC_WAVE_CLI", cfg.cli());
    if item.kind == QueueKind::Qa
        && std::env::var("SPEC_WAVE_QA_CMD").map(|c| c.trim().is_empty()).unwrap_or(true)
        && !hub_has_qa_command(ws)
    {
        cmd.env("SPEC_WAVE_QA_CMD", &cfg.qa_executor);
    }
    #[cfg(unix)]
    cmd.process_group(0); // líder de grupo => kill_tree pega a árvore toda
    let mut child = cmd.spawn()
        .with_context(|| format!("falha ao iniciar executor {:?}", argv[0]))?;
    // Anexa a árvore ANTES de o filho ter tempo de spawnar netos.
    let tree = ProcessTree::attach(&child);

    // Prompt via stdin (fechado em seguida — o executor lê até EOF).
    if let Some(mut si) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        si.write_all(prompt.as_bytes()).await
            .context("falha ao escrever o prompt no stdin do executor")?;
    }

    // Os handles saem do child via take(), então kill_tree nos braços
    // abaixo continua imediato — fencing não espera os readers.
    let out_task = child.stdout.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "out", live.map(|h| h.tap))));
    let err_task = child.stderr.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "err", None)));

    let cap = TokioDuration::from_secs(timeout_secs);
    let end = tokio::select! {
        status = timeout(cap, child.wait()) => match status {
            Err(_) => {
                kill_tree(&mut child, &tree).await;
                RoundRaw::Failed(format!("timeout de {}s", cap.as_secs()))
            }
            Ok(Ok(st)) if st.success() => RoundRaw::Success,
            Ok(Ok(st)) => RoundRaw::Failed(match st.code() {
                Some(c) => format!("exit code {c}"),
                None => format!("encerrado por sinal ({st})"),
            }),
            Ok(Err(e)) => RoundRaw::Failed(e.to_string()),
        },
        _ = lease_lost.changed() => {
            // Fencing: perdemos o lease => outro agente pode estar ativo.
            // Matar imediatamente, SEM push (o novo dono manda no branch).
            kill_tree(&mut child, &tree).await;
            RoundRaw::LeaseLost
        }
        _ = shutdown.changed() => {
            kill_tree(&mut child, &tree).await;
            RoundRaw::Interrupted
        }
    };

    // Mesmo em saída natural do executor, mata retardatários da árvore: um
    // orquestrador que "termina" deixando implements em background não pode
    // deixar claudes órfãos trabalhando.
    tree.kill();

    // Drena o resto do output (após kill/exit os pipes fecham em EOF).
    for t in [out_task, err_task].into_iter().flatten() {
        let _ = timeout(TokioDuration::from_secs(2), t).await;
    }

    if item.kind == QueueKind::Flow {
        return Ok(flow_round_end(end));
    }
    if item.kind == QueueKind::Qa {
        let outcome_path = ws.join(crate::config::QA_OUTCOME_FILE);
        let raw = std::fs::read_to_string(&outcome_path).ok();
        let _ = std::fs::remove_file(&outcome_path);
        return Ok(match end {
            RoundRaw::LeaseLost => RoundEnd::LeaseLost,
            RoundRaw::Interrupted => RoundEnd::Interrupted,
            RoundRaw::Success => qa_round_end(raw.as_deref(), "exit 0"),
            RoundRaw::Failed(r) => qa_round_end(raw.as_deref(), &r),
        });
    }

    // Bloqueio por dependência vence o marker e o exit code: o executor pode
    // ter escrito "partial" (ou nada, ou saído com erro ao ver o exit 3 do
    // implement) — o registro da CLI é a fonte determinística.
    if reads_blocked && matches!(end, RoundRaw::Success | RoundRaw::Failed(_)) {
        if let Some(rec) = take_blocked_record(ws).await {
            let _ = take_result_marker(ws);
            info!(target: "agent", "#{issue}: {}", rec.describe());
            return Ok(RoundEnd::Blocked(rec));
        }
    }

    // Exit 0 só vale como sucesso com o marker "ok" do executor; sem
    // marker é "rodada incompleta" (o run_feature relança para continuar).
    let end = match end {
        RoundRaw::Success => match take_result_marker(ws) {
            Some(r) if r.status == "ok" => RoundEnd::Success(Some(r)),
            Some(r) if r.status == "blocked" => RoundEnd::Stopped(
                r.detalhe.filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| "o executor parou sem dizer o impedimento".into())),
            Some(r) => RoundEnd::Failed(format!(
                "executor terminou sem concluir tudo (status {:?}{})",
                r.status,
                r.detalhe.clone().map(|d| format!(": {d}")).unwrap_or_default())),
            None => RoundEnd::Incomplete,
        },
        RoundRaw::Failed(r) => {
            let _ = take_result_marker(ws); // não deixar sujar o checkpoint
            RoundEnd::Failed(r)
        }
        RoundRaw::LeaseLost => RoundEnd::LeaseLost,
        RoundRaw::Interrupted => {
            let _ = take_result_marker(ws);
            RoundEnd::Interrupted
        }
    };
    Ok(end)
}

async fn head_sha(ws: &Path) -> String {
    run(ws, "git", &["rev-parse", "HEAD"]).await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default()
}

/// Executa a feature em RODADAS do executor: exit 0 sem marker = o
/// orquestrador encerrou o turno com trabalho pendente (ex: aguardando
/// implements longos) => relança para continuar do estado do git/spec-wave.
/// Estagnação (2 rodadas seguidas sem commit novo) ou estouro de
/// max_executor_rounds => falha.
/// Roda um item da fila até o desfecho. O TIPO escolhe comando, prompt e
/// timeout; todo o resto — rodadas, checkpoint, fencing, kill_tree — é
/// idêntico, e continua sendo, de propósito: a diferença entre corrigir um bug
/// e implementar uma feature é o que se pede ao executor, não como se o
/// supervisiona.
pub async fn run_item(
    cfg: &Config, ws: &Path, item: QueueItem,
    lease_lost: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
) -> Result<RunEnd> {
    run_item_with_tap(cfg, ws, item, lease_lost, shutdown, None).await
}

/// `run_item` com a saída do executor copiada para a transmissão ao vivo.
pub async fn run_item_with_tap(
    cfg: &Config, ws: &Path, item: QueueItem,
    lease_lost: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
    live: Option<LiveHooks>,
) -> Result<RunEnd> {
    run_rounds(cfg, ws, item, lease_lost, shutdown, live, None).await
}

async fn run_rounds(
    cfg: &Config, ws: &Path, item: QueueItem,
    lease_lost: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
    live: Option<LiveHooks>,
    extra_prompt: Option<&str>,
) -> Result<RunEnd> {
    let issue = item.number;
    let mut no_progress = 0u32;
    for round in 1..=cfg.max_executor_rounds {
        let head_before = head_sha(ws).await;
        if let Some(h) = &live {
            h.round.store(round, std::sync::atomic::Ordering::SeqCst);
        }
        let end = executor_round(cfg, ws, item, lease_lost.clone(), shutdown.clone(),
                                 live.clone(), extra_prompt).await?;
        match end {
            RoundEnd::Success(r) => return Ok(RunEnd::Success(r)),
            RoundEnd::Failed(r) => return Ok(RunEnd::Failed(r)),
            RoundEnd::Blocked(b) => return Ok(RunEnd::Blocked(b)),
            RoundEnd::Stopped(r) => return Ok(RunEnd::Stopped(r)),
            RoundEnd::LeaseLost => return Ok(RunEnd::LeaseLost),
            RoundEnd::Interrupted => return Ok(RunEnd::Interrupted),
            RoundEnd::Incomplete => {
                // Numa revisão o HEAD do hub não mede progresso: o trabalho é
                // comentar no PR e commitar na branch dele, muitas vezes num
                // clone de `.spec-wave/repos/`. O teto de rodadas ainda vale.
                let progressed = matches!(item.kind, QueueKind::Review | QueueKind::Qa | QueueKind::Flow)
                    || head_sha(ws).await != head_before;
                if progressed {
                    no_progress = 0;
                } else {
                    no_progress += 1;
                    if no_progress >= 2 {
                        return Ok(RunEnd::Failed(format!(
                            "{no_progress} rodadas seguidas sem progresso \
                             (sem commits novos) e sem marker de conclusão")));
                    }
                }
                info!(target: "agent",
                      "feature #{issue}: rodada {round} terminou sem marker; \
                       relançando executor para continuar (progresso: {progressed})");
            }
        }
    }
    Ok(RunEnd::Failed(format!(
        "max_executor_rounds ({}) atingido sem marker de conclusão",
        cfg.max_executor_rounds)))
}

async fn all_heads(workspace: &Workspace) -> Vec<String> {
    let mut heads = vec![head_sha(&workspace.hub).await];
    for cr in &workspace.code_repos {
        heads.push(head_sha(&cr.dir).await);
    }
    heads
}

/// Desfecho de `run_item_gated`: o do executor + os PRs que o gate abriu.
///
/// Os PRs vêm junto em QUALQUER desfecho: com o gate o PR nasce antes do
/// veredito, então uma execução que falha por CI vermelho deixa um PR aberto
/// — e quem reporta precisa poder dizer qual.
#[derive(Debug)]
pub struct Gated {
    pub end: RunEnd,
    pub prs: Vec<String>,
}

/// Revisão dos PRs de uma Feature (`QueueKind::Review`): as mesmas rodadas do
/// executor, com o prompt de revisão — e NADA do desfecho de implementação.
///
/// Não abre PR (a revisão trabalha nos que já existem) e não passa pelo gate
/// de CI (uma rodada só: o que o CI disser das correções é assunto do humano
/// que vai aprovar). No sucesso faz o checkpoint — cinto de segurança para uma
/// correção que o executor commitou e não pushou (`push_all`, sem commit de
/// sobras) — e devolve os PRs que o
/// executor declarou ter revisado no marker. Lista vazia = não havia PR
/// elegível; quem chama decide o desfecho.
pub async fn run_item_review(
    cfg: &Config, workspace: &Workspace, item: QueueItem,
    lease_lost: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
    live: Option<LiveHooks>,
) -> Result<Gated> {
    let end = run_item_with_tap(cfg, &workspace.hub, item, lease_lost, shutdown, live).await?;
    let mut prs = Vec::new();
    if let RunEnd::Success(result) = &end {
        prs = review_prs(result);
        push_all(workspace).await;
    }
    Ok(Gated { end, prs })
}

/// Pusha o que já está COMMITADO em cada clone — sem `add`/`commit`.
///
/// A revisão trabalha na branch de um Pull Request que um humano vai aprovar:
/// o cinto de segurança dela é só garantir o push dos commits de correção que
/// o executor fez (um por achado, testados). Um checkpoint "wip" com o que
/// sobrou sem commit empurraria edição pela metade para dentro do PR.
pub async fn push_all(workspace: &Workspace) {
    let _ = run(&workspace.hub, "git", &["push", "--quiet"]).await;
    for cr in &workspace.code_repos {
        let _ = run(&cr.dir, "git", &["push", "--quiet"]).await;
    }
}

/// PRs que o executor de revisão declarou no marker — só URLs http(s), sem
/// repetição (o texto vem de um modelo).
pub fn review_prs(result: &Option<ExecResult>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in result.as_ref().and_then(|r| r.prs.clone()).unwrap_or_default() {
        let url = url.trim().to_string();
        if (url.starts_with("https://") || url.starts_with("http://")) && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// `run_item_with_tap` + desfecho de sucesso (checkpoint e PR) + GATE DE CI:
/// o marker "ok" do executor só vira sucesso depois que os checks do PR
/// confirmam.
///
/// A cada "ok": checkpoint, PR (idempotente) e espera pelos checks do commit
/// de cabeça de cada PR. Check vermelho relança o executor com os checks
/// reprovados e o log resumido no prompt — até `max_ci_fix_rounds` vezes;
/// depois disso (ou se a rodada de correção não produzir commit) a execução
/// FALHA, com o PR aberto e o motivo dizendo o que reprovou.
///
/// Roda com o lease ainda nas mãos (o heartbeat de quem chama continua vivo):
/// perda de lease ou interrupção durante a espera saem pelos mesmos desfechos
/// do executor. CI que não dá para consultar, sem checks ou lento demais NÃO
/// reprova — o gate confere o que o repositório diz, não inventa veredito.
///
/// O `ExecResult` devolvido é o do PRIMEIRO "ok": num bug é ele que carrega a
/// causa raiz, e o marker de uma rodada de correção de CI fala de outra coisa.
pub async fn run_item_gated(
    cfg: &Config, workspace: &Workspace, item: QueueItem,
    lease_lost: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
    live: Option<LiveHooks>,
    pr_command: &[String],
) -> Result<Gated> {
    use crate::ci::{self, CiWait, RedPr};
    let issue = item.number;
    let mut extra: Option<String> = None;
    let mut fixes = 0u32;
    let mut kept: Option<ExecResult> = None;
    let mut urls: Vec<String> = Vec::new();
    loop {
        let heads_before = all_heads(workspace).await;
        let end = run_rounds(cfg, &workspace.hub, item, lease_lost.clone(), shutdown.clone(),
                             live.clone(), extra.as_deref()).await?;
        match end {
            RunEnd::Success(r) => if kept.is_none() { kept = r; },
            RunEnd::Blocked(b) => {
                // O que já foi feito fica no branch para a retomada; PR NÃO:
                // abrir PR leva a Feature para Code Review pela metade.
                checkpoint_all(workspace, issue, "blocked").await;
                return Ok(Gated { end: RunEnd::Blocked(b), prs: urls });
            }
            RunEnd::Stopped(r) => {
                // Idem: o que rodou fica no branch, sem PR pela metade.
                checkpoint_all(workspace, issue, "stopped").await;
                return Ok(Gated { end: RunEnd::Stopped(r), prs: urls });
            }
            other => return Ok(Gated { end: other, prs: urls }),
        }
        // Cinto de segurança: garante push de qualquer resto que o executor
        // não tenha commitado — os checks rodam sobre o que está no remoto.
        checkpoint_all(workspace, issue, "success").await;
        if extra.is_some() && all_heads(workspace).await == heads_before {
            return Ok(Gated {
                end: RunEnd::Failed(
                    "CI do PR reprovado e a rodada de correção terminou sem commit novo".into()),
                prs: urls,
            });
        }
        let prs = open_pull_requests_with_dirs(workspace, pr_command, &cfg.repo, issue, cfg.pr_draft).await;
        urls = prs.iter().map(|(url, _)| url.clone()).collect();
        if !cfg.ci_gate {
            return Ok(Gated { end: RunEnd::Success(kept), prs: urls });
        }
        // Um relógio só para todos os PRs da issue (multi-repo).
        let started = std::time::Instant::now();
        let mut red: Vec<RedPr> = Vec::new();
        for (url, dir) in &prs {
            let failed = match ci::wait_checks(cfg, dir, url, started,
                                               lease_lost.clone(), shutdown.clone()).await {
                CiWait::Green => {
                    info!(target: "ci", "#{issue}: checks verdes em {url}");
                    continue;
                }
                CiWait::NoChecks => {
                    info!(target: "ci", "#{issue}: {url} não tem checks — nada a conferir");
                    continue;
                }
                CiWait::TimedOut => {
                    warn!(target: "ci",
                        "#{issue}: checks de {url} ainda em andamento após {}s — seguindo sem veredito",
                        cfg.ci_timeout_secs);
                    continue;
                }
                CiWait::Unavailable(why) => {
                    warn!(target: "ci",
                        "#{issue}: não foi possível consultar os checks de {url} ({why}) — gate pulado");
                    continue;
                }
                CiWait::LeaseLost => return Ok(Gated { end: RunEnd::LeaseLost, prs: urls }),
                CiWait::Interrupted => return Ok(Gated { end: RunEnd::Interrupted, prs: urls }),
                CiWait::Red(failed) => failed,
            };
            let mut checks = Vec::with_capacity(failed.len());
            for check in failed {
                let log = match ci::failure_log(dir, &check, lease_lost.clone(), shutdown.clone()).await {
                    Ok(log) => log,
                    Err(CiWait::LeaseLost) => return Ok(Gated { end: RunEnd::LeaseLost, prs: urls }),
                    Err(_) => return Ok(Gated { end: RunEnd::Interrupted, prs: urls }),
                };
                checks.push((check, log));
            }
            red.push(RedPr { url: url.clone(), checks });
        }
        if red.is_empty() {
            return Ok(Gated { end: RunEnd::Success(kept), prs: urls });
        }
        let summary = ci::summarize(&red);
        if fixes >= cfg.max_ci_fix_rounds {
            return Ok(Gated {
                end: RunEnd::Failed(format!(
                    "CI do PR reprovado após {fixes} rodada(s) de correção — {summary}")),
                prs: urls,
            });
        }
        fixes += 1;
        warn!(target: "ci",
              "#{issue}: CI reprovado ({summary}); relançando o executor para corrigir \
               (correção {fixes}/{})", cfg.max_ci_fix_rounds);
        extra = Some(ci::fix_prompt(&red));
    }
}

#[cfg(test)]
mod tests {
    fn qa(raw: Option<&str>) -> (String, String) {
        match super::qa_round_end(raw, "exit code 1") {
            super::RoundEnd::Success(Some(r)) => (r.status, r.detalhe.unwrap_or_default()),
            super::RoundEnd::Failed(r) => ("failed".into(), r),
            _ => ("outro".into(), String::new()),
        }
    }

    #[test]
    fn qa_desfecho_pelo_arquivo_e_nao_pelo_exit_code() {
        assert_eq!(qa(Some(r#"{"outcome":"pass","bugs":[],"pending":[]}"#)),
                   ("ok".into(), "QA verde".into()));
        // Reprova: o QA TERMINOU (os Bugs já foram abertos) — não é falha da frota.
        assert_eq!(qa(Some(r#"{"outcome":"fail","bugs":[91,"92"]}"#)),
                   ("ok".into(), "QA reprovou: Bug(s) #91, #92".into()));
        assert_eq!(qa(Some(r#"{"outcome":"blocked"}"#)).0, "blocked");
        assert_eq!(qa(Some(r#"{"outcome":"pass","pending":[3]}"#)).0, "blocked");
        assert_eq!(qa(Some(r#"{"outcome":"not-configured","reason":"sem executor"}"#)),
                   ("blocked".into(), "sem executor".into()));
        assert_eq!(qa(Some(r#"{"outcome":"refused","reason":"sem qa-ready"}"#)).0, "blocked");
        assert_eq!(qa(Some(r#"{"outcome":"skipped"}"#)).0, "ok");
        assert_eq!(qa(Some(r#"{"outcome":"error","reason":"boom"}"#)), ("failed".into(), "boom".into()));
        let (s, r) = qa(None);
        assert_eq!(s, "failed");
        assert!(r.contains("sem desfecho"), "{r}");
        assert_eq!(qa(Some("não é json")).0, "failed");
    }

    #[test]
    fn flow_desfecho_pelo_exit_code() {
        let st = |raw| match super::flow_round_end(raw) {
            super::RoundEnd::Success(Some(r)) => format!("ok:{}", r.detalhe.unwrap_or_default()),
            super::RoundEnd::Failed(r) => format!("failed:{r}"),
            _ => "outro".into(),
        };
        assert_eq!(st(super::RoundRaw::Success), "ok:etapa concluída");
        assert!(st(super::RoundRaw::Failed("exit code 2".into())).starts_with("ok:etapa concluída com recusa"));
        assert_eq!(st(super::RoundRaw::Failed("exit code 1".into())), "failed:exit code 1");
        assert_eq!(st(super::RoundRaw::Failed("timeout de 60s".into())), "failed:timeout de 60s");
    }

    #[test]
    fn registro_de_bloqueio_v1_e_lido_e_descrito() {
        let r = parse_blocked_record(r#"{"version":1,"issue":40,"reason":"waiting_dependency",
            "waitingOn":[12,15],"blocked":[{"story":41,"waitingOn":[12,15]}],"runnable":2,
            "at":"2026-11-24T00:00:00Z"}"#).expect("v1 válido");
        assert_eq!(r.waiting_on, vec![12, 15]);
        assert_eq!(r.runnable, 2);
        assert_eq!(r.blocked, vec![BlockedStory { story: 41, waiting_on: vec![12, 15] }]);
        assert_eq!(r.describe(),
            "bloqueada por dependência: Story #41 aguarda #12, #15 — o código ainda não está na base");
    }

    #[test]
    fn registro_de_bloqueio_desconhecido_nao_bloqueia() {
        assert!(parse_blocked_record(r#"{"version":2,"reason":"waiting_dependency"}"#).is_none());
        assert!(parse_blocked_record(r#"{"version":1,"reason":" "}"#).is_none());
        assert!(parse_blocked_record("não é json").is_none());
        // Campos opcionais ausentes: ainda é bloqueio, com descrição genérica.
        let r = parse_blocked_record(r#"{"version":1,"reason":"waiting_dependency","waitingOn":[9]}"#).unwrap();
        assert_eq!(r.describe(), "bloqueada por dependência: aguardando #9 — o código ainda não está na base");
    }

    #[test]
    fn hub_com_e_sem_qa_command() {
        let dir = std::env::temp_dir().join(format!("swa-qa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!super::hub_has_qa_command(&dir));
        std::fs::write(dir.join(".spec-wave.json"), r#"{"qa":{"command":"run-qa {contextFile}"}}"#).unwrap();
        assert!(super::hub_has_qa_command(&dir));
        std::fs::write(dir.join(".spec-wave.json"), r#"{"qa":{"command":"  "}}"#).unwrap();
        assert!(!super::hub_has_qa_command(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_prs_so_urls_sem_repeticao() {
        let r: ExecResult = serde_json::from_str(
            r#"{"status":"ok","prs":[" https://github.com/a/b/pull/1 ","nenhum","https://github.com/a/b/pull/1","http://ghe.local/a/b/pull/2"]}"#,
        ).unwrap();
        assert_eq!(super::review_prs(&Some(r)),
                   vec!["https://github.com/a/b/pull/1".to_string(), "http://ghe.local/a/b/pull/2".to_string()]);
        // Marker antigo (sem o campo) e ausência de marker: lista vazia.
        let antigo: ExecResult = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert!(super::review_prs(&Some(antigo)).is_empty());
        assert!(super::review_prs(&None).is_empty());
    }

    /// A régua é a EXPLICIT_LINK_RE da CLI (lib/pr-link.mjs): verbo
    /// `implement(s|ed)` + espaço + `owner/repo#N`. Em português
    /// ("Implementa a issue") não casa e o board não anda no merge.
    #[test]
    fn corpo_do_pr_abre_com_vinculo_que_a_cli_reconhece() {
        let body = super::pr_body("acme/hub", 386);
        assert!(body.starts_with("Implements acme/hub#386."), "{body}");
        // Nada de palavra-chave de fechamento: o merge não encerra a issue.
        let lower = body.to_lowercase();
        for kw in ["closes", "fixes", "resolves"] {
            assert!(!lower.contains(kw), "{kw} fecharia a issue no merge");
        }
    }

    use super::{parse_cli_pr, CliPr};

    #[test]
    fn cli_pr_ok_devolve_a_url_do_json() {
        let out = "aviso qualquer\n{\"url\":\"https://scm/pull/3\",\"number\":\"3\",\"created\":false}\n";
        assert_eq!(parse_cli_pr(Some(0), out, ""), CliPr::Opened("https://scm/pull/3".into()));
    }

    #[test]
    fn cli_pr_codigo_2_e_nada_a_revisar() {
        assert_eq!(parse_cli_pr(Some(2), "", "nada à frente"), CliPr::NothingToReview);
    }

    #[test]
    fn cli_pr_antiga_ou_com_erro_cai_no_gh() {
        // CLI anterior à 1.7.0: commander recusa o comando.
        assert!(matches!(parse_cli_pr(Some(1), "", "error: unknown command 'pr'"),
                         CliPr::Unavailable(w) if w.contains("unknown command")));
        // Exit 0 sem JSON (ex.: um npx que não é a CLI).
        assert!(matches!(parse_cli_pr(Some(0), "10.0.0\n", ""), CliPr::Unavailable(_)));
        assert!(matches!(parse_cli_pr(None, "", ""), CliPr::Unavailable(_)));
    }

    use super::*;

    fn render(line: &str) -> Option<String> {
        render_out_line(line)
    }

    /// Contrato com `spec-wave workspace prepare --json` (commands/workspace.mjs
    /// `emit()`, spec-wave-cli) — campos extras (workspaceDir, cloned, missing...)
    /// não usados aqui são ignorados por padrão (sem `deny_unknown_fields`), então
    /// o formato pode crescer do lado da CLI sem quebrar o parse.
    #[test]
    fn parseia_a_saida_do_workspace_prepare() {
        let raw = r#"{
            "issue": 7, "dryRun": false,
            "workspaceDir": "/home/dev/proj", "hubDir": "/home/dev/proj/hub",
            "repos": [
                {"slug": "api", "owner": "acme", "repo": "acme/api",
                 "dir": "/home/dev/proj/api", "source": "clone", "present": true},
                {"slug": "web", "owner": "acme", "repo": "acme/web",
                 "dir": "/home/dev/proj/web", "source": "sibling", "present": true}
            ],
            "repoDirs": "api=/home/dev/proj/api web=/home/dev/proj/web",
            "cloned": ["api"], "missing": []
        }"#;
        let parsed: WorkspacePrepareOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.repos.len(), 2);
        assert_eq!(parsed.repos[0].repo, "acme/api");
        assert_eq!(parsed.repos[0].dir, "/home/dev/proj/api");
        assert_eq!(parsed.repos[1].repo, "acme/web");
    }

    #[test]
    fn workspace_prepare_sem_code_repos_devolve_lista_vazia() {
        let raw = r#"{"issue": 7, "dryRun": false, "workspaceDir": "/p", "hubDir": "/p/hub",
                       "repos": [], "repoDirs": "", "cloned": [], "missing": []}"#;
        let parsed: WorkspacePrepareOutput = serde_json::from_str(raw).unwrap();
        assert!(parsed.repos.is_empty());
    }

    #[test]
    fn assistant_texto_e_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Vou rodar o order."},
            {"type":"tool_use","name":"Bash","input":{"command":"npx spec-wave order 7"}}
        ]}}"#.replace('\n', "");
        let msg = render(&line).unwrap();
        assert!(msg.contains("Vou rodar o order."));
        assert!(msg.contains("⏵ Bash: npx spec-wave order 7"));
    }

    #[test]
    fn tool_use_edit_mostra_arquivo() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Edit","input":{"file_path":"src/auth.ts","old_string":"x"}}
        ]}}"#.replace('\n', "");
        assert_eq!(render(&line).unwrap(), "⏵ Edit: src/auth.ts");
    }

    #[test]
    fn system_sem_init_e_suprimido() {
        assert!(render(r#"{"type":"system","subtype":"status"}"#).is_none());
        let msg = render(r#"{"type":"system","subtype":"init","model":"claude-x"}"#).unwrap();
        assert_eq!(msg, "sessão iniciada (modelo claude-x)");
    }

    #[test]
    fn resultado_com_custo() {
        let line = r#"{"type":"result","subtype":"success","total_cost_usd":1.234,"num_turns":42}"#;
        assert_eq!(render(line).unwrap(), "fim: success, custo US$1.23, 42 turnos");
    }

    #[test]
    fn tool_result_e_suprimido() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"saida gigante"}]}}"#;
        assert!(render(line).is_none());
    }

    #[test]
    fn linha_nao_json_passa_crua() {
        assert_eq!(render("progresso 42%").unwrap(), "progresso 42%");
        // JSON sem "type" (ex: log de outra ferramenta) também passa cru
        assert_eq!(render(r#"{"foo": 1}"#).unwrap(), r#"{"foo": 1}"#);
    }

    #[test]
    fn truncagem_segura_com_utf8() {
        let s = "ação".repeat(100);
        let t = trunc(&s, 10);
        assert_eq!(t.chars().count(), 11); // 10 + reticências
    }
}
