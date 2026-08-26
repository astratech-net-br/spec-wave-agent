//! Fila (labels = UX; a correção está no lease).

use crate::config::Config;
use crate::shell::{retry_backoff, run_ok};
use anyhow::Result;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

#[derive(Deserialize)]
struct GhLabel { name: String }
#[derive(Deserialize)]
struct GhIssue { number: u64, labels: Vec<GhLabel> }
#[derive(Deserialize)]
struct GhIssueLabels { labels: Vec<GhLabel> }

/// O que o agente pode puxar da fila. A ORDEM DAS VARIANTES é a ordem de
/// prioridade: `derive(Ord)` compara pela posição na declaração, e
/// `parse_queue` ordena por `(kind, number)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QueueKind {
    /// Trabalho corretivo. Vem PRIMEIRO: um defeito com severidade não pode
    /// esperar atrás de feature nova numa FIFO por número.
    Bug,
    Feature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueueItem {
    /// Campos nesta ordem porque `derive(Ord)` é lexicográfico: primeiro o
    /// tipo (bug antes de feature), depois o número (FIFO dentro do tipo).
    pub kind: QueueKind,
    pub number: u64,
}

/// Filtra o JSON do `gh issue list`: só issues tipadas [FEATURE] ou [BUG].
/// Bugs primeiro, FIFO por número dentro de cada tipo.
///
/// A label da fila vai na Feature (o executor implementa as stories filhas na
/// ordem de dependência) ou no Bug (o executor corrige o defeito). Story e Task
/// avulsas continuam fora: quem as coordena é o `spec-wave implement` da
/// Feature.
pub fn parse_queue(json: &str) -> Result<Vec<QueueItem>> {
    let issues: Vec<GhIssue> = serde_json::from_str(json)?;
    let mut items: Vec<QueueItem> = issues.into_iter()
        .filter_map(|i| kind_of(&i.labels).map(|kind| QueueItem { kind, number: i.number }))
        .collect();
    items.sort_unstable();
    Ok(items)
}

/// Tipo da issue pelas labels. Com AMBAS as labels vence `Bug` — na dúvida, o
/// caminho mais restritivo (o prompt de bug investiga antes de mexer).
fn kind_of(labels: &[GhLabel]) -> Option<QueueKind> {
    let has = |n: &str| labels.iter().any(|l| l.name == n);
    if has("[BUG]") {
        Some(QueueKind::Bug)
    } else if has("[FEATURE]") {
        Some(QueueKind::Feature)
    } else {
        None
    }
}

pub async fn poll_queue(cfg: &Config, cwd: &Path) -> Result<Vec<QueueItem>> {
    let out = retry_backoff("poll da fila", 3, Duration::from_secs(2), || {
        let repo = cfg.repo.clone();
        let label = cfg.queue_label.clone();
        let cwd = cwd.to_path_buf();
        async move {
            run_ok(&cwd, "gh",
                &["issue", "list", "--repo", &repo,
                  "--label", &label, "--state", "open",
                  "--json", "number,labels", "--limit", "50"]).await
        }
    }).await?;
    parse_queue(&out)
}

/// Próximo intervalo de poll, dado quantas rodadas VAZIAS seguidas já
/// aconteceram (função PURA) — item 4 do rfc/plano-hardening-agentes-
/// 2026-08.md.
///
/// `vazios_seguidos == 1` (a primeira rodada vazia) ainda usa `base`, sem
/// backoff — é o comportamento de hoje. A partir da segunda, dobra
/// (`base × 2^(vazios_seguidos − 1)`), até `max`. `vazios_seguidos == 0`
/// (chamada fora do caso de uso pretendido) também devolve `base`, por
/// segurança.
///
/// Fila cheia sem claim (outro agente levou tudo) e erro de poll não entram
/// aqui — só quem chama decide incrementar `vazios_seguidos`, e só faz isso
/// quando o poll devolveu uma fila genuinamente vazia.
pub fn next_poll_delay(vazios_seguidos: u32, base: u64, max: u64) -> u64 {
    let expoente = vazios_seguidos.saturating_sub(1);
    let mult = 1u64.checked_shl(expoente).unwrap_or(u64::MAX);
    base.saturating_mul(mult).min(max.max(base))
}

/// A label da fila está presente, pelo JSON de `gh issue view --json labels`
/// (função PURA).
fn label_present(json: &str, label: &str) -> Result<bool> {
    let parsed: GhIssueLabels = serde_json::from_str(json)?;
    Ok(parsed.labels.iter().any(|l| l.name == label))
}

/// Confirma AGORA, com uma leitura fresca (`gh issue view`, não `gh issue
/// list`), se a label da fila está na issue — item 2 do rfc/plano-hardening-
/// agentes-2026-08.md.
///
/// Existe porque `gh issue list` (o `poll_queue` acima) consulta o ÍNDICE DE
/// BUSCA do GitHub, que atrasa depois de uma remoção de label; `gh issue
/// view` lê a issue direto, sem esse atraso. É o que permite distinguir
/// "índice atrasado — a label já saiu de verdade" de "um humano reaplicou a
/// label" para uma issue da qual o agente desistiu.
pub async fn label_present_now(cfg: &Config, cwd: &Path, issue: u64) -> Result<bool> {
    let out = run_ok(cwd, "gh",
        &["issue", "view", &issue.to_string(), "--repo", &cfg.repo,
          "--json", "labels"]).await?;
    label_present(&out, &cfg.queue_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(number: u64) -> QueueItem { QueueItem { kind: QueueKind::Feature, number } }
    fn bug(number: u64) -> QueueItem { QueueItem { kind: QueueKind::Bug, number } }

    #[test]
    fn filtra_features_e_ordena() {
        let json = r#"[
            {"number": 30, "labels": [{"name": "spec-wave:dev-agent"}, {"name": "[FEATURE]"}]},
            {"number": 5,  "labels": [{"name": "spec-wave:dev-agent"}, {"name": "[STORY]"}]},
            {"number": 12, "labels": [{"name": "spec-wave:dev-agent"}, {"name": "[TASK]"}]},
            {"number": 8,  "labels": [{"name": "[FEATURE]"}, {"name": "extra"}]}
        ]"#;
        // [STORY]/[TASK] avulsas NÃO entram na fila
        assert_eq!(parse_queue(json).unwrap(), vec![feature(8), feature(30)]);
    }

    #[test]
    fn bugs_vem_antes_de_features() {
        // Trabalho corretivo tem severidade; feature nova, não. Uma FIFO pura
        // por número deixaria um bug crítico atrás de qualquer feature antiga.
        let json = r#"[
            {"number": 3,  "labels": [{"name": "[FEATURE]"}]},
            {"number": 90, "labels": [{"name": "[BUG]"}]},
            {"number": 10, "labels": [{"name": "[FEATURE]"}]},
            {"number": 91, "labels": [{"name": "[BUG]"}]}
        ]"#;
        assert_eq!(
            parse_queue(json).unwrap(),
            vec![bug(90), bug(91), feature(3), feature(10)]
        );
    }

    #[test]
    fn item_com_ambas_as_labels_e_bug() {
        // Na dúvida, o caminho mais restritivo: o prompt de bug investiga antes
        // de mexer no código.
        let json = r#"[{"number": 7, "labels": [{"name": "[FEATURE]"}, {"name": "[BUG]"}]}]"#;
        assert_eq!(parse_queue(json).unwrap(), vec![bug(7)]);
    }

    #[test]
    fn lista_vazia_e_sem_labels() {
        assert_eq!(parse_queue("[]").unwrap(), Vec::<QueueItem>::new());
        let json = r#"[{"number": 1, "labels": []}]"#;
        assert_eq!(parse_queue(json).unwrap(), Vec::<QueueItem>::new());
    }

    #[test]
    fn json_invalido_erra() {
        assert!(parse_queue("not json").is_err());
    }

    // --- next_poll_delay (item 4) --------------------------------------

    #[test]
    fn primeira_rodada_vazia_ainda_usa_a_base() {
        assert_eq!(next_poll_delay(1, 60, 480), 60);
    }

    #[test]
    fn dobra_a_cada_rodada_vazia_seguinte() {
        assert_eq!(next_poll_delay(2, 60, 480), 120);
        assert_eq!(next_poll_delay(3, 60, 480), 240);
        assert_eq!(next_poll_delay(4, 60, 480), 480);
    }

    #[test]
    fn teto_nao_e_ultrapassado() {
        assert_eq!(next_poll_delay(5, 60, 480), 480);
        assert_eq!(next_poll_delay(20, 60, 480), 480);
    }

    #[test]
    fn vazios_seguidos_zero_devolve_a_base() {
        assert_eq!(next_poll_delay(0, 60, 480), 60);
    }

    #[test]
    fn teto_menor_que_a_base_nao_produz_delay_menor_que_a_base() {
        // config.validate() já recusa isso, mas a função pura não deve
        // devolver um delay abaixo da base por causa de um teto mal
        // configurado.
        assert_eq!(next_poll_delay(1, 60, 10), 60);
    }

    // --- label_present (item 2) -----------------------------------------

    #[test]
    fn label_present_encontra_a_label_certa() {
        let json = r#"{"labels":[{"name":"spec-wave:dev-agent"},{"name":"[FEATURE]"}]}"#;
        assert!(label_present(json, "spec-wave:dev-agent").unwrap());
        assert!(!label_present(json, "spec-wave:dev-agent-b").unwrap());
    }

    #[test]
    fn label_present_sem_labels() {
        assert!(!label_present(r#"{"labels":[]}"#, "spec-wave:dev-agent").unwrap());
    }

    #[test]
    fn label_present_json_invalido_erra() {
        assert!(label_present("not json", "spec-wave:dev-agent").is_err());
    }
}
