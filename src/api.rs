//! Cliente da API do agente no spec-wave (`source = "api"`, RFC-008 fase 2).
//!
//! Substitui a fila de labels: o agente pergunta à API o que está em WIP para
//! o login do dono do token, reporta heartbeat (e recebe a ordem de parar
//! quando o despacho é cancelado) e reporta o desfecho. A correção continua no
//! lease em git ref — a API decide O QUE fazer, o lease garante UM dono.

use crate::config::{hostname, Config};
use crate::queue::{QueueItem, QueueKind};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Um item em WIP despachado para este agente.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkItem {
    pub repo_id: String,
    /// Hub "owner/repo" do produto.
    pub repo: String,
    #[serde(default)]
    pub code_repos: Vec<String>,
    pub work_item: String,
    pub kind: String,
    pub run_id: String,
    pub dispatched_at: String,
}

#[derive(Debug, Deserialize)]
struct WorkResponse {
    login: String,
    items: Vec<WorkItem>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct HeartbeatReply {
    #[serde(rename = "continue")]
    pub keep_going: bool,
    pub reason: Option<String>,
}

/// Desfecho reportado ao spec-wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Trabalho concluído: o card vai para Review.
    Succeeded,
    /// Falhou, mas pode ser transitório: continua em WIP (a API conta).
    Failed(String),
    /// O agente desistiu: o card vai para Blocked com o motivo.
    Blocked(String),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeBody<'a> {
    run_id: &'a str,
    outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatBody<'a> {
    run_id: &'a str,
    host: &'a str,
}

/// Converte um item da API para o item da fila local. `None` para o que o
/// agente não sabe executar (tipo desconhecido ou chave que não é número de
/// issue) — a API pode evoluir antes do agente.
pub fn queue_item_of(w: &WorkItem) -> Option<QueueItem> {
    let number: u64 = w.work_item.parse().ok().filter(|n| *n > 0)?;
    let kind = match w.kind.as_str() {
        "bug" => QueueKind::Bug,
        "feature" => QueueKind::Feature,
        _ => return None,
    };
    Some(QueueItem { kind, number })
}

fn outcome_body<'a>(run_id: &'a str, outcome: &'a Outcome) -> OutcomeBody<'a> {
    match outcome {
        Outcome::Succeeded => OutcomeBody { run_id, outcome: "succeeded", reason: None },
        Outcome::Failed(r) => OutcomeBody { run_id, outcome: "failed", reason: Some(r) },
        Outcome::Blocked(r) => OutcomeBody { run_id, outcome: "blocked", reason: Some(r) },
    }
}

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    token: String,
    host: String,
    live_base: Option<String>,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn from_config(cfg: &Config, me: &str) -> Result<ApiClient> {
        // rustls sem provedor embutido no reqwest: instala o `ring` uma vez
        // (compila sem cmake/NASM nas três plataformas do release).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let base = cfg.api_url.as_deref().map(str::trim).context("api_url ausente")?;
        let token = cfg.agent_token().context("agent_token ausente")?;
        let http = reqwest::Client::builder()
            // O WAF do spec-wave bloqueia requisição sem User-Agent.
            .user_agent(concat!("spec-wave-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()?;
        let host = if me.contains('@') { me.to_string() } else { format!("{me}@{}", hostname()) };
        let live_base = cfg.live_url.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(str::to_string);
        Ok(ApiClient { base: base.trim_end_matches('/').to_string(), token, host, live_base, http })
    }

    /// Abre a transmissão ao vivo da execução para o Gateway (live.rs).
    pub fn start_live(&self, w: &WorkItem) -> Option<crate::live::LiveStream> {
        crate::live::LiveStream::start(self.live_base.as_deref().unwrap_or(&self.base), &self.token, w)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1{path}", self.base)
    }

    fn dispatch_url(&self, w: &WorkItem, action: &str) -> String {
        self.url(&format!("/dispatches/{}/{}/{action}", w.repo_id, w.work_item))
    }

    /// Itens em WIP despachados para este agente, na ordem da fila (bugs
    /// primeiro, depois quem foi despachado antes). Também registra presença.
    pub async fn fetch_work(&self) -> Result<(String, Vec<WorkItem>)> {
        let res = self.http.get(self.url("/work"))
            .bearer_auth(&self.token)
            .header("X-Agent-Host", &self.host)
            .send().await
            .context("API do spec-wave inacessível")?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("token de agente inválido ou revogado — gere outro em Configurações → Minha conta");
        }
        if !status.is_success() {
            bail!("GET /work respondeu {status}: {}", error_text(res).await);
        }
        let body: WorkResponse = res.json().await.context("resposta de /work inválida")?;
        Ok((body.login, body.items))
    }

    /// Heartbeat da tentativa. `keep_going = false` = o despacho foi
    /// cancelado, refeito ou redirecionado: o agente para.
    pub async fn heartbeat(&self, w: &WorkItem) -> Result<HeartbeatReply> {
        let res = self.http.post(self.dispatch_url(w, "heartbeat"))
            .bearer_auth(&self.token)
            .json(&HeartbeatBody { run_id: &w.run_id, host: &self.host })
            .send().await?;
        let status = res.status();
        if !status.is_success() {
            bail!("heartbeat respondeu {status}: {}", error_text(res).await);
        }
        Ok(res.json().await?)
    }

    /// Desfecho. Um 409 (o despacho mudou enquanto o agente trabalhava) não é
    /// erro do agente: vira aviso e o trabalho já pushado fica no branch.
    pub async fn report(&self, w: &WorkItem, outcome: &Outcome) -> Result<()> {
        let res = self.http.post(self.dispatch_url(w, "outcome"))
            .bearer_auth(&self.token)
            .json(&outcome_body(&w.run_id, outcome))
            .send().await?;
        let status = res.status();
        if status == reqwest::StatusCode::CONFLICT {
            tracing::warn!(target: "api", "#{}: desfecho ignorado pelo spec-wave: {}",
                           w.work_item, error_text(res).await);
            return Ok(());
        }
        if !status.is_success() {
            bail!("outcome respondeu {status}: {}", error_text(res).await);
        }
        Ok(())
    }
}

async fn error_text(res: reqwest::Response) -> String {
    #[derive(Deserialize)]
    struct Err { error: String }
    let raw = res.text().await.unwrap_or_default();
    serde_json::from_str::<Err>(&raw).map(|e| e.error).unwrap_or(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(kind: &str, key: &str) -> WorkItem {
        WorkItem {
            repo_id: "01REPO".into(),
            repo: "acme/produto".into(),
            code_repos: vec![],
            work_item: key.into(),
            kind: kind.into(),
            run_id: "01RUN".into(),
            dispatched_at: "2026-09-26T10:00:00Z".into(),
        }
    }

    #[test]
    fn converte_item_da_api_para_a_fila() {
        assert_eq!(queue_item_of(&work("bug", "12")),
                   Some(QueueItem { kind: QueueKind::Bug, number: 12 }));
        assert_eq!(queue_item_of(&work("feature", "7")),
                   Some(QueueItem { kind: QueueKind::Feature, number: 7 }));
    }

    #[test]
    fn ignora_o_que_nao_sabe_executar() {
        assert_eq!(queue_item_of(&work("spike", "12")), None);
        assert_eq!(queue_item_of(&work("feature", "PROJ-12")), None);
        assert_eq!(queue_item_of(&work("feature", "0")), None);
    }

    #[test]
    fn corpo_do_desfecho() {
        let ok = serde_json::to_value(outcome_body("R1", &Outcome::Succeeded)).unwrap();
        assert_eq!(ok, serde_json::json!({ "runId": "R1", "outcome": "succeeded" }));
        let bl = serde_json::to_value(outcome_body("R1", &Outcome::Blocked("3 falhas".into()))).unwrap();
        assert_eq!(bl, serde_json::json!({ "runId": "R1", "outcome": "blocked", "reason": "3 falhas" }));
    }

    #[test]
    fn le_a_resposta_de_work_e_de_heartbeat() {
        let body: WorkResponse = serde_json::from_str(r#"{
            "login": "ana",
            "items": [{ "repoId": "01REPO", "repo": "acme/produto", "codeRepos": ["acme/api"],
                        "workItem": "12", "kind": "bug", "runId": "01RUN",
                        "dispatchedAt": "2026-09-26T10:00:00Z" }]
        }"#).unwrap();
        assert_eq!(body.login, "ana");
        assert_eq!(body.items[0].code_repos, vec!["acme/api".to_string()]);
        let hb: HeartbeatReply = serde_json::from_str(r#"{"continue":false,"reason":"cancelado"}"#).unwrap();
        assert_eq!(hb, HeartbeatReply { keep_going: false, reason: Some("cancelado".into()) });
    }
}
