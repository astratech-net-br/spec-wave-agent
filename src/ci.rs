//! Gate de CI: consulta os checks do PR, espera o veredito do commit de
//! cabeça e monta o contexto da falha para a rodada de correção do executor.
//!
//! O marker "ok" é a palavra do executor. Este módulo é a conferência: o CI só
//! roda depois que o PR abre, então sem esperar por ele o agente declarava
//! sucesso em trabalho que o próprio repositório reprovava minutos depois.

use crate::config::Config;
use crate::shell::run;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// Um check que reprovou.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    pub link: Option<String>,
}

/// Veredito dos checks de UM commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Pending,
    Red(Vec<FailedCheck>),
    /// Nenhum check registrado (ainda, ou o repositório não tem CI).
    NoChecks,
}

/// Uma consulta aos checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// O PR ainda aponta para outro commit — o push não foi processado. Os
    /// checks listados são do commit ANTIGO e não dizem nada sobre o atual.
    Stale,
    Verdict(Verdict),
}

/// Desfecho da espera pelos checks de um PR.
#[derive(Debug, PartialEq, Eq)]
pub enum CiWait {
    Green,
    Red(Vec<FailedCheck>),
    NoChecks,
    /// Checks em andamento além de `ci_timeout_secs`.
    TimedOut,
    /// Não deu para consultar (comando ausente, credencial, SCM sem suporte).
    Unavailable(String),
    LeaseLost,
    Interrupted,
}

enum State { Pass, Pending, Fail }

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Classifica um item do `statusCheckRollup`: CheckRun (`status` +
/// `conclusion`) ou StatusContext (`state`). Cancelado/pulado/neutro não
/// reprovam — não há nada que o executor possa corrigir neles.
fn classify(item: &Value) -> State {
    if let Some(status) = text(item, "status") {
        if !status.eq_ignore_ascii_case("COMPLETED") {
            return State::Pending;
        }
        return match text(item, "conclusion").unwrap_or("").to_ascii_uppercase().as_str() {
            "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" => State::Fail,
            _ => State::Pass,
        };
    }
    match text(item, "state").unwrap_or("").to_ascii_uppercase().as_str() {
        "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" => State::Pending,
        "FAILURE" | "ERROR" => State::Fail,
        _ => State::Pass,
    }
}

fn check_name(item: &Value) -> String {
    let name = text(item, "name").or_else(|| text(item, "context")).unwrap_or("check sem nome");
    match text(item, "workflowName") {
        Some(w) if w != name => format!("{w} / {name}"),
        _ => name.to_string(),
    }
}

/// Interpreta `{"headRefOid": "...", "statusCheckRollup": [...]}` (o JSON do
/// `gh pr view --json headRefOid,statusCheckRollup`). Pura, para os testes.
///
/// Vermelho vence pendente: um check reprovado já basta para agir, sem
/// esperar os outros terminarem. `headRefOid` ausente (comando customizado
/// que não o informa) não é tratado como divergência.
pub fn parse_checks(stdout: &str, head: &str) -> Option<Probe> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    if !v.is_object() {
        return None;
    }
    if let Some(oid) = text(&v, "headRefOid") {
        if !head.is_empty() && oid != head {
            return Some(Probe::Stale);
        }
    }
    let items = v.get("statusCheckRollup").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Some(Probe::Verdict(Verdict::NoChecks));
    }
    let mut failed = Vec::new();
    let mut pending = false;
    for item in &items {
        match classify(item) {
            State::Fail => failed.push(FailedCheck {
                name: check_name(item),
                link: text(item, "detailsUrl").or_else(|| text(item, "targetUrl")).map(str::to_string),
            }),
            State::Pending => pending = true,
            State::Pass => {}
        }
    }
    Some(Probe::Verdict(if !failed.is_empty() {
        Verdict::Red(failed)
    } else if pending {
        Verdict::Pending
    } else {
        Verdict::Green
    }))
}

async fn head_sha(dir: &Path) -> String {
    run(dir, "git", &["rev-parse", "HEAD"]).await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default()
}

async fn probe(dir: &Path, argv: &[String], url: &str) -> Result<Probe, String> {
    let Some((program, base)) = argv.split_first() else {
        return Err("checks_command vazio".into());
    };
    let mut args: Vec<&str> = base.iter().map(String::as_str).collect();
    args.extend([url, "--json", "headRefOid,statusCheckRollup"]);
    let out = run(dir, program, &args).await.map_err(|e| format!("{e:#}"))?;
    if !out.ok {
        let why = out.stderr.trim().lines().last().unwrap_or("").to_string();
        return Err(if why.is_empty() { format!("código {:?}", out.code) } else { why });
    }
    parse_checks(&out.stdout, &head_sha(dir).await)
        .ok_or_else(|| format!("saída inesperada: {}", trunc(out.stdout.trim(), 200)))
}

/// Dorme `d`, acordando antes se o lease cair ou o agente for interrompido.
async fn pause(
    d: Duration, lease_lost: &mut watch::Receiver<bool>, shutdown: &mut watch::Receiver<bool>,
) -> Option<CiWait> {
    if *lease_lost.borrow() { return Some(CiWait::LeaseLost); }
    if *shutdown.borrow() { return Some(CiWait::Interrupted); }
    tokio::select! {
        _ = tokio::time::sleep(d) => None,
        _ = lease_lost.changed() => Some(CiWait::LeaseLost),
        _ = shutdown.changed() => Some(CiWait::Interrupted),
    }
}

/// Espera o veredito dos checks do PR `url` para o HEAD de `dir`.
///
/// - Verde só vale em DUAS consultas seguidas: workflows registram seus checks
///   em momentos diferentes, e um que termina rápido não pode liberar o gate
///   antes de o mais lento sequer aparecer.
/// - Sem checks (ou PR ainda no commit antigo) além de
///   `ci_no_checks_grace_secs`: o repositório não tem CI para este PR.
/// - Falha na PRIMEIRA consulta desiste na hora (comando ausente/sem
///   credencial é o caso comum); depois, tolera 3 falhas seguidas (rede).
pub async fn wait_checks(
    cfg: &Config, dir: &Path, url: &str,
    mut lease_lost: watch::Receiver<bool>, mut shutdown: watch::Receiver<bool>,
) -> CiWait {
    let argv = match crate::config::split_command(&cfg.checks_command) {
        Ok(a) => a,
        Err(e) => return CiWait::Unavailable(format!("{e:#}")),
    };
    let start = Instant::now();
    let poll = Duration::from_secs(cfg.ci_poll_secs.max(1));
    let grace = Duration::from_secs(cfg.ci_no_checks_grace_secs);
    let cap = Duration::from_secs(cfg.ci_timeout_secs);
    let (mut greens, mut errors, mut first) = (0u32, 0u32, true);
    info!(target: "ci", "aguardando os checks de {url}");
    loop {
        match probe(dir, &argv, url).await {
            Err(why) => {
                errors += 1;
                if first || errors >= 3 {
                    return CiWait::Unavailable(why);
                }
                warn!(target: "ci", "consulta aos checks falhou ({why}); nova tentativa");
            }
            Ok(p) => {
                errors = 0;
                match p {
                    Probe::Verdict(Verdict::Red(failed)) => return CiWait::Red(failed),
                    Probe::Verdict(Verdict::Green) => {
                        greens += 1;
                        if greens >= 2 { return CiWait::Green; }
                    }
                    Probe::Verdict(Verdict::Pending) => greens = 0,
                    Probe::Verdict(Verdict::NoChecks) | Probe::Stale => {
                        greens = 0;
                        if start.elapsed() >= grace {
                            return match p {
                                Probe::Stale => CiWait::Unavailable(
                                    "o PR não aponta para o commit de cabeça local".into()),
                                _ => CiWait::NoChecks,
                            };
                        }
                    }
                }
            }
        }
        first = false;
        if start.elapsed() >= cap {
            return CiWait::TimedOut;
        }
        if let Some(end) = pause(poll, &mut lease_lost, &mut shutdown).await {
            return end;
        }
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// `(owner/repo, job id)` de um link de job do GitHub Actions
/// (`https://github.com/<o>/<r>/actions/runs/<run>/job/<job>`).
pub fn actions_job(link: &str) -> Option<(String, String)> {
    let rest = link.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split(['/', '?', '#']).collect();
    match parts.as_slice() {
        [owner, repo, "actions", "runs", _run, "job", job, ..]
            if !job.is_empty() && job.chars().all(|c| c.is_ascii_digit()) =>
            Some((format!("{owner}/{repo}"), job.to_string())),
        _ => None,
    }
}

const MAX_SIGN_LINES: usize = 60;
const CONTEXT: usize = 6;
const TAIL: usize = 60;
const MAX_LOG_CHARS: usize = 8000;

/// A linha ABRE um relato de falha? Só marcas de início de linha: procurar
/// "error"/"failed" em qualquer posição casa com título de teste verde
/// ("ok 12 - falha vira job error") e afoga o que importa.
fn is_failure_line(line: &str) -> bool {
    let t = line.trim_start().trim_start_matches("# ").trim_start();
    ["not ok", "FAIL", "error", "Error", "ERROR", "✗", "✖", "--- FAIL", "E   "]
        .iter().any(|p| t.starts_with(p))
        || ["##[error]", "panicked at", "AssertionError"].iter().any(|p| t.contains(p))
}

/// Reduz o log de um job reprovado ao que cabe num prompt: cada linha que
/// abre uma falha com as seguintes (o detalhe vem logo abaixo) e o final do
/// log. Tira o prefixo `job<TAB>step<TAB>` e o timestamp de cada linha.
pub fn digest_log(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().map(|l| {
        let msg = l.rsplit('\t').next().unwrap_or(l);
        match msg.split_once(' ') {
            Some((ts, rest)) if ts.ends_with('Z') && ts.starts_with(|c: char| c.is_ascii_digit()) => rest,
            _ => msg,
        }
    }).filter(|l| !l.trim().is_empty()).collect();
    let tail_from = lines.len().saturating_sub(TAIL);
    let mut signs: Vec<&str> = Vec::new();
    let mut keep_until = 0usize;
    for (i, l) in lines[..tail_from].iter().enumerate() {
        if is_failure_line(l) {
            if i > keep_until && !signs.is_empty() {
                signs.push("…");
            }
            keep_until = i + CONTEXT;
        }
        if i <= keep_until && (keep_until > 0 || is_failure_line(l)) {
            signs.push(l);
            if signs.len() >= MAX_SIGN_LINES { break; }
        }
    }
    let mut out = String::new();
    if !signs.is_empty() {
        out.push_str("Trechos com falha:\n");
        for l in signs {
            out.push_str(&trunc(l.trim_end(), 300));
            out.push('\n');
        }
        out.push_str("\nFim do log:\n");
    }
    for l in &lines[tail_from..] {
        out.push_str(&trunc(l.trim_end(), 300));
        out.push('\n');
    }
    trunc(out.trim_end(), MAX_LOG_CHARS)
}

/// Log resumido de um check reprovado — só para jobs do GitHub Actions, via
/// `gh`. Outro provedor de CI (ou falha ao baixar) fica só com o nome e o link.
pub async fn failure_log(dir: &Path, check: &FailedCheck) -> Option<String> {
    let (repo, job) = actions_job(check.link.as_deref()?)?;
    let out = run(dir, "gh", &["run", "view", "--repo", &repo, "--job", &job, "--log-failed"])
        .await.ok()?;
    let log = digest_log(&out.stdout);
    (out.ok && !log.is_empty()).then_some(log)
}

/// PR reprovado: URL, e cada check com o log resumido (quando há).
pub struct RedPr {
    pub url: String,
    pub checks: Vec<(FailedCheck, Option<String>)>,
}

/// Uma linha para logs e para o motivo da falha: `<url>: check a, check b`.
pub fn summarize(red: &[RedPr]) -> String {
    red.iter().map(|pr| {
        let names: Vec<&str> = pr.checks.iter().map(|(c, _)| c.name.as_str()).collect();
        format!("{}: {}", pr.url, names.join(", "))
    }).collect::<Vec<_>>().join("; ")
}

/// Seção acrescentada ao prompt do executor na rodada de correção de CI.
pub fn fix_prompt(red: &[RedPr]) -> String {
    let mut p = String::from(
        "\n\n=== CORREÇÃO DE CI ===\n\
         O trabalho desta issue já foi commitado, pushado e o PR está aberto — \
         mas o CI do PR REPROVOU. Esta rodada existe só para corrigir isso. NÃO \
         reimplemente stories nem refaça o que já está commitado.\n\
         1. Leia os checks reprovados e os logs abaixo e ache a causa.\n\
         2. Corrija a causa — no código OU no teste, o que estiver errado. \
         Não desligue, pule nem apague um teste para o CI passar.\n\
         3. Rode a suíte completa localmente e confirme que o que reprovou \
         agora passa.\n\
         4. Commite a correção neste branch (assunto começando com \
         \"fix(ci):\"; numa Feature, repita no rodapé o trailer Spec-Wave-Story \
         do commit da story dona do código corrigido) e faça push.\n\
         5. Só então escreva o marker de conclusão. O agente vai esperar o CI \
         de novo; sem commit novo o CI não muda e a execução falha.\n\
         Se a falha não tiver relação com este trabalho (infra do CI, teste \
         instável que já falhava na base) e você não puder corrigi-la, escreva \
         o marker com {\"status\": \"partial\", \"detalhe\": \"<por quê>\"}.\n");
    for pr in red {
        p.push_str(&format!("\nPR: {}\n", pr.url));
        for (check, log) in &pr.checks {
            p.push_str(&format!("\nCheck reprovado: {}\n", check.name));
            if let Some(link) = &check.link {
                p.push_str(&format!("Link: {link}\n"));
            }
            match log {
                Some(log) => p.push_str(&format!("Log (resumido):\n{log}\n")),
                None => p.push_str("Log indisponível — reproduza rodando a suíte localmente.\n"),
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(json: &str) -> Probe {
        parse_checks(json, "abc").expect("json válido")
    }

    #[test]
    fn rollup_do_gh_vermelho_com_pulado_e_verde() {
        // Formato real do `gh pr view --json headRefOid,statusCheckRollup`.
        let json = r#"{"headRefOid":"abc","statusCheckRollup":[
          {"__typename":"CheckRun","name":"move","workflowName":"Code Review","status":"COMPLETED","conclusion":"SKIPPED","detailsUrl":"https://github.com/o/r/actions/runs/1/job/2"},
          {"__typename":"CheckRun","name":"ci","workflowName":"ci","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://github.com/o/r/actions/runs/3/job/4"},
          {"__typename":"CheckRun","name":"resolve","workflowName":"Code Review","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/o/r/actions/runs/1/job/5"}]}"#;
        assert_eq!(probe_of(json), Probe::Verdict(Verdict::Red(vec![FailedCheck {
            name: "ci".into(),
            link: Some("https://github.com/o/r/actions/runs/3/job/4".into()),
        }])));
    }

    #[test]
    fn pendente_verde_e_sem_checks() {
        assert_eq!(probe_of(r#"{"headRefOid":"abc","statusCheckRollup":[
            {"name":"a","status":"COMPLETED","conclusion":"SUCCESS"},
            {"name":"b","status":"IN_PROGRESS","conclusion":""}]}"#),
            Probe::Verdict(Verdict::Pending));
        assert_eq!(probe_of(r#"{"headRefOid":"abc","statusCheckRollup":[
            {"name":"a","status":"COMPLETED","conclusion":"SUCCESS"},
            {"name":"b","status":"COMPLETED","conclusion":"CANCELLED"},
            {"context":"deploy/preview","state":"SUCCESS"}]}"#),
            Probe::Verdict(Verdict::Green));
        assert_eq!(probe_of(r#"{"headRefOid":"abc","statusCheckRollup":[]}"#),
            Probe::Verdict(Verdict::NoChecks));
        assert_eq!(probe_of(r#"{"headRefOid":"abc"}"#), Probe::Verdict(Verdict::NoChecks));
    }

    #[test]
    fn vermelho_vence_pendente_e_status_context_reprova() {
        let p = probe_of(r#"{"statusCheckRollup":[
            {"name":"lento","status":"QUEUED"},
            {"context":"ci/externo","state":"ERROR","targetUrl":"https://ci.example/9"}]}"#);
        assert_eq!(p, Probe::Verdict(Verdict::Red(vec![FailedCheck {
            name: "ci/externo".into(), link: Some("https://ci.example/9".into()),
        }])));
    }

    #[test]
    fn pr_em_outro_commit_e_stale() {
        // Os checks listados são do commit antigo: nem o vermelho deles conta.
        let json = r#"{"headRefOid":"velho","statusCheckRollup":[
            {"name":"ci","status":"COMPLETED","conclusion":"FAILURE"}]}"#;
        assert_eq!(probe_of(json), Probe::Stale);
        // Sem HEAD local conhecido não há como comparar.
        assert!(matches!(parse_checks(json, ""), Some(Probe::Verdict(Verdict::Red(_)))));
    }

    #[test]
    fn saida_que_nao_e_o_json_esperado() {
        assert_eq!(parse_checks("", "abc"), None);
        assert_eq!(parse_checks("no checks reported", "abc"), None);
        assert_eq!(parse_checks("[]", "abc"), None);
    }

    #[test]
    fn job_do_actions_a_partir_do_link() {
        assert_eq!(actions_job("https://github.com/acme/web/actions/runs/44185269462/job/134400012293"),
                   Some(("acme/web".into(), "134400012293".into())));
        assert_eq!(actions_job("https://github.com/acme/web/actions/runs/1/job/2?pr=3"),
                   Some(("acme/web".into(), "2".into())));
        assert_eq!(actions_job("https://github.com/acme/web/runs/77"), None);
        assert_eq!(actions_job("https://ci.example/build/9"), None);
    }

    #[test]
    fn digest_guarda_os_sinais_de_falha_e_o_fim() {
        let mut raw = String::new();
        for i in 0..500 {
            raw.push_str(&format!("ci\tTestes\t2026-11-17T22:00:18.70Z ok {i} - teste {i}\n"));
            if i == 100 {
                raw.push_str("ci\tTestes\t2026-11-17T22:00:18.71Z not ok 5 - classifica timeout do cliente\n");
                raw.push_str("ci\tTestes\t2026-11-17T22:00:18.72Z   error: 'Promise resolution is still pending'\n");
            }
        }
        raw.push_str("ci\tTestes\t2026-11-17T22:00:18.73Z ##[error]Process completed with exit code 1.\n");
        let d = digest_log(&raw);
        assert!(d.contains("not ok 5 - classifica timeout do cliente"), "{d}");
        assert!(d.contains("Promise resolution is still pending"));
        assert!(d.contains("Process completed with exit code 1."));
        assert!(!d.contains("2026-11-17T22"), "timestamp deveria sair");
        assert!(!d.contains("ok 7 - teste 7"), "linhas verdes do meio não entram");
        // Título de teste VERDE com "error"/"falhou" no nome não é falha.
        let verde = "ci\tT\t2026-11-17T22:00:18.70Z ok 3 - run: CLI sai com error e failed\n".repeat(200);
        assert!(!digest_log(&verde).contains("Trechos com falha"));
        assert!(d.chars().count() <= MAX_LOG_CHARS + 1);
    }

    #[test]
    fn prompt_de_correcao_leva_pr_check_e_log() {
        let red = vec![RedPr {
            url: "https://github.com/o/r/pull/9".into(),
            checks: vec![
                (FailedCheck { name: "ci".into(), link: Some("https://x/1".into()) }, Some("not ok 5".into())),
                (FailedCheck { name: "lint".into(), link: None }, None),
            ],
        }];
        let p = fix_prompt(&red);
        for needle in ["CORREÇÃO DE CI", "https://github.com/o/r/pull/9", "Check reprovado: ci",
                       "not ok 5", "Check reprovado: lint", "Log indisponível"] {
            assert!(p.contains(needle), "faltou {needle:?} em: {p}");
        }
        assert_eq!(summarize(&red), "https://github.com/o/r/pull/9: ci, lint");
    }
}
