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

/// Filtra o JSON do `gh issue list`: só issues tipadas [STORY]/[TASK],
/// FIFO por número.
pub fn parse_queue(json: &str) -> Result<Vec<u64>> {
    let issues: Vec<GhIssue> = serde_json::from_str(json)?;
    let mut nums: Vec<u64> = issues.into_iter()
        .filter(|i| i.labels.iter()
            .any(|l| l.name == "[STORY]" || l.name == "[TASK]"))
        .map(|i| i.number)
        .collect();
    nums.sort_unstable();
    Ok(nums)
}

pub async fn poll_queue(cfg: &Config, cwd: &Path) -> Result<Vec<u64>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtra_tipos_e_ordena() {
        let json = r#"[
            {"number": 30, "labels": [{"name": "spec-wave:dev-agent"}, {"name": "[TASK]"}]},
            {"number": 5,  "labels": [{"name": "spec-wave:dev-agent"}, {"name": "[STORY]"}]},
            {"number": 12, "labels": [{"name": "spec-wave:dev-agent"}, {"name": "bug"}]},
            {"number": 8,  "labels": [{"name": "[TASK]"}, {"name": "extra"}]}
        ]"#;
        assert_eq!(parse_queue(json).unwrap(), vec![5, 8, 30]);
    }

    #[test]
    fn lista_vazia_e_sem_labels() {
        assert_eq!(parse_queue("[]").unwrap(), Vec::<u64>::new());
        let json = r#"[{"number": 1, "labels": []}]"#;
        assert_eq!(parse_queue(json).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn json_invalido_erra() {
        assert!(parse_queue("not json").is_err());
    }
}
