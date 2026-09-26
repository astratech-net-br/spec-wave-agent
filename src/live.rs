//! Transmissão ao vivo da sessão para o spec-wave (RFC-008 fase 3).
//!
//! Na fonte `api`, o agente manda a saída `stream-json` do executor para o
//! Session Gateway, e a tela Development mostra a sessão num drawer, somente
//! leitura. O Gateway confere o token de agente na API do spec-wave antes de
//! aceitar a conexão — o agente não afirma quem é, só apresenta o token.
//!
//! Regra de ouro: a transmissão é MELHOR ESFORÇO. Ela nunca bloqueia, atrasa
//! ou derruba o executor — o que importa é o trabalho no branch. Sem Gateway,
//! com rede ruim ou recusado, o agente segue igual e o drawer só fica sem
//! mensagens.

use crate::api::WorkItem;
use crate::runner::{LiveHooks, LiveLine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// Linhas guardadas enquanto o Gateway está fora (reconectando). Passando
/// disso, as mais novas são descartadas — o executor nunca espera.
const QUEUE: usize = 4096;
/// O Gateway derruba o produtor sem nada por ~60 s: ping a cada 25 s.
const PING_EVERY: Duration = Duration::from_secs(25);

/// Um frame do protocolo do Gateway (envelope `{v,type,id,ts,payload}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub kind: &'static str,
    pub payload: Value,
}

/// Converte UMA linha do `stream-json` do Claude Code nos frames que o Agent
/// Console sabe renderizar — a mesma normalização do `session-runner`
/// (`normalize.ts` no spec-wave-sandbox). O que o console não conhece é
/// descartado aqui. Linha que não é JSON (executor de outro agente, log solto)
/// vira nada: o console só mostra eventos estruturados.
pub fn normalize(line: &str) -> Vec<Frame> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    let msg = |msg_type: &str, payload: Value| Frame {
        kind: "session.message",
        payload: json!({ "msg_type": msg_type, "payload": payload }),
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            vec![msg("system", json!({
                "subtype": "init",
                "model": v.get("model"),
                "tools": v.get("tools"),
                "permission_mode": v.get("permissionMode"),
            }))]
        }
        Some("assistant") => match v.get("message") {
            Some(m) => vec![msg("assistant", json!({ "message": m }))],
            None => vec![],
        },
        // Só o `user` que carrega tool_result: é ele que dá a saída aos cards
        // de ferramenta. O prompt do usuário não trafega (não há usuário).
        Some("user") => {
            let has_result = v.pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|c| c.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")));
            if has_result {
                vec![msg("user", json!({ "message": v.get("message"), "synthetic": true }))]
            } else {
                vec![]
            }
        }
        Some("result") => vec![Frame {
            kind: "session.result",
            payload: json!({
                "subtype": v.get("subtype").and_then(Value::as_str).unwrap_or("success"),
                "duration_ms": v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
                "num_turns": v.get("num_turns").and_then(Value::as_i64).unwrap_or(0),
                "cost_usd": v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                "is_error": v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            }),
        }],
        Some("stream_event") => {
            let text = v.pointer("/event/delta")
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("text_delta"))
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str);
            match text {
                Some(t) => vec![Frame { kind: "session.delta", payload: json!({ "text": t }) }],
                None => vec![],
            }
        }
        _ => vec![],
    }
}

/// `normalize` de uma linha com origem. Linhas de uma story (`story:<n>`)
/// levam a origem dentro do payload — a tela separa a sessão por story — e não
/// emitem `result` nem `delta`: o resultado de uma story não é o fim da sessão,
/// e o texto parcial dela se misturaria ao do orquestrador.
pub fn normalize_line(l: &LiveLine) -> Vec<Frame> {
    let frames = normalize(&l.line);
    let Some(origin) = &l.origin else { return frames };
    frames
        .into_iter()
        .filter(|f| f.kind == "session.message")
        .map(|mut f| {
            if let Some(inner) = f.payload.get_mut("payload").and_then(Value::as_object_mut) {
                inner.insert("origin".into(), json!(origin));
            }
            f
        })
        .collect()
}

/// Nome do arquivo de stream de uma story → origem (`story-92.jsonl` → `story:92`).
fn origin_of(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    let n = name.strip_prefix("story-")?.strip_suffix(".jsonl")?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| format!("story:{n}"))
}

/// Uma passada pelos arquivos de story: manda as linhas COMPLETAS novas desde a
/// última leitura. Linha sem `\n` ainda está sendo escrita — fica para a
/// próxima passada.
fn scan_stories(dir: &Path, offsets: &mut HashMap<PathBuf, u64>, tx: &mpsc::Sender<LiveLine>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    for file in files {
        let Some(origin) = origin_of(&file) else { continue };
        let offset = offsets.entry(file.clone()).or_insert(0);
        let Ok(mut f) = std::fs::File::open(&file) else { continue };
        if f.seek(SeekFrom::Start(*offset)).is_err() {
            continue;
        }
        // Em bytes: um caractere multibyte cortado no fim (linha ainda sendo
        // escrita) ou um byte inválido não pode travar o arquivo inteiro.
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            continue;
        }
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { continue };
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            let _ = tx.try_send(LiveLine { origin: Some(origin.clone()), line: line.to_string() });
        }
        *offset += (end + 1) as u64;
    }
}

/// Acompanha o diretório de stream das stories até `stop`, com uma passada
/// final para não perder o fim da última story.
async fn tail_stories(dir: PathBuf, tx: mpsc::Sender<LiveLine>, mut stop: watch::Receiver<bool>) {
    let mut offsets = HashMap::new();
    let mut tick = interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tick.tick() => scan_stories(&dir, &mut offsets, &tx),
            _ = stop.changed() => {
                scan_stories(&dir, &mut offsets, &tx);
                return;
            }
        }
    }
}

/// URL do produtor no Gateway, derivada da `api_url`: o Gateway fica atrás do
/// mesmo domínio (o CloudFront manda `/ws/*` para ele).
pub fn producer_url(api_url: &str, w: &WorkItem) -> Option<String> {
    let base = reqwest::Url::parse(api_url.trim()).ok()?;
    let scheme = match base.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return None,
    };
    let host = base.host_str()?;
    let port = base.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!(
        "{scheme}://{host}{port}/ws/dev-agent/producer?repoId={}&workItem={}&runId={}",
        w.repo_id, w.work_item, w.run_id
    ))
}

fn envelope(kind: &str, payload: &Value, seq: u64) -> String {
    json!({
        "v": 1,
        "type": kind,
        "id": format!("{}-{seq}", std::process::id()),
        "ts": chrono::Utc::now().to_rfc3339(),
        "payload": payload,
    })
    .to_string()
}

/// Transmissão de UMA execução. Mande as linhas cruas de stdout do executor
/// em `tap()`; ao terminar a execução, `finish()`.
pub struct LiveStream {
    tx: mpsc::Sender<LiveLine>,
    task: JoinHandle<()>,
    stream_dir: PathBuf,
    tail: JoinHandle<()>,
    stop_tail: watch::Sender<bool>,
}

impl LiveStream {
    /// `stream_dir`: onde o `spec-wave implement` grava o stream de cada story
    /// (o executor recebe o caminho em SPEC_WAVE_STREAM_DIR).
    pub fn start(api_url: &str, token: &str, work: &WorkItem, stream_dir: PathBuf) -> Option<LiveStream> {
        let url = producer_url(api_url, work)?;
        let (tx, rx) = mpsc::channel(QUEUE);
        let task = tokio::spawn(run(url, token.to_string(), work.work_item.clone(), rx));
        let (stop_tail, stop_rx) = watch::channel(false);
        let tail = tokio::spawn(tail_stories(stream_dir.clone(), tx.clone(), stop_rx));
        Some(LiveStream { tx, task, stream_dir, tail, stop_tail })
    }

    /// Ganchos para o runner. `try_send`: cheio = descarta, nunca espera.
    pub fn hooks(&self) -> LiveHooks {
        LiveHooks { tap: self.tx.clone(), stream_dir: self.stream_dir.clone() }
    }

    /// Lê o fim das stories, fecha o canal e dá um instante para o resto sair;
    /// depois encerra.
    pub async fn finish(self) {
        let _ = self.stop_tail.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), self.tail).await;
        drop(self.tx);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
            task.abort();
        }
    }
}

async fn run(url: String, token: String, item: String, mut rx: mpsc::Receiver<LiveLine>) {
    let mut backoff = Duration::from_secs(1);
    let mut seq: u64 = 0;
    let mut pending: Option<LiveLine> = None; // linha lida mas não enviada (queda no meio)
    loop {
        let mut req = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(_) => return,
        };
        let headers = req.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert("Authorization", v);
        }
        headers.insert("User-Agent", HeaderValue::from_static(concat!("spec-wave-agent/", env!("CARGO_PKG_VERSION"))));

        let ws = match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => ws,
            Err(tokio_tungstenite::tungstenite::Error::Http(res)) => {
                let status = res.status().as_u16();
                // Recusa definitiva: token ruim ou execução que não vale mais.
                if matches!(status, 400 | 401 | 403 | 409) {
                    tracing::warn!(target: "live", "#{item}: transmissão ao vivo recusada pelo Gateway (HTTP {status}) — o trabalho segue sem ela");
                    while rx.recv().await.is_some() {} // drena até o fim da execução
                    return;
                }
                tracing::debug!(target: "live", "#{item}: Gateway respondeu {status}; nova tentativa");
                if !wait_or_end(&mut rx, &mut backoff).await { return; }
                continue;
            }
            Err(e) => {
                tracing::debug!(target: "live", "#{item}: conexão ao vivo falhou: {e}");
                if !wait_or_end(&mut rx, &mut backoff).await { return; }
                continue;
            }
        };
        tracing::info!(target: "live", "#{item}: transmissão ao vivo conectada");
        backoff = Duration::from_secs(1);
        let (mut sink, mut stream) = ws.split();
        let mut ping = interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let line = match pending.take() {
                Some(l) => Some(l),
                None => tokio::select! {
                    l = rx.recv() => match l {
                        Some(l) => Some(l),
                        None => {
                            // Fim da execução: avisa que encerrou (senão o
                            // drawer mostraria "Agente desconectado", como se
                            // o agente tivesse caído) e fecha limpo.
                            seq += 1;
                            let done = json!({ "status": "closed", "detail": "Execução encerrada" });
                            let _ = sink.send(Message::Text(envelope("session.status", &done, seq).into())).await;
                            let _ = sink.send(Message::Close(None)).await;
                            return;
                        }
                    },
                    _ = ping.tick() => {
                        seq += 1;
                        if sink.send(Message::Text(envelope("heartbeat.ping", &json!({}), seq).into())).await.is_err() {
                            break;
                        }
                        None
                    }
                    incoming = stream.next() => match incoming {
                        Some(Ok(_)) => None, // pong / status: nada a fazer
                        _ => break,          // Gateway fechou
                    },
                },
            };
            let Some(line) = line else { continue };
            let mut dropped = false;
            for f in normalize_line(&line) {
                seq += 1;
                if sink.send(Message::Text(envelope(f.kind, &f.payload, seq).into())).await.is_err() {
                    dropped = true;
                    break;
                }
            }
            if dropped {
                pending = Some(line); // reenvia na próxima conexão
                break;
            }
        }
        tracing::info!(target: "live", "#{item}: transmissão ao vivo caiu; reconectando");
        if !wait_or_end(&mut rx, &mut backoff).await { return; }
    }
}

/// Espera o backoff. Enquanto espera, o canal continua enchendo (até o teto).
/// false = a execução acabou (canal fechado) e não há por que reconectar.
async fn wait_or_end(rx: &mut mpsc::Receiver<LiveLine>, backoff: &mut Duration) -> bool {
    sleep(*backoff).await;
    *backoff = (*backoff * 2).min(Duration::from_secs(30));
    !rx.is_closed() || !rx.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> WorkItem {
        WorkItem {
            repo_id: "01REPO".into(), repo: "acme/p".into(), code_repos: vec![],
            work_item: "12".into(), kind: "feature".into(), run_id: "01RUN".into(),
            dispatched_at: "2026-09-26T10:00:00Z".into(),
        }
    }

    #[test]
    fn url_do_produtor_vem_da_api_url() {
        assert_eq!(producer_url("https://app.dev/agent-api", &work()).unwrap(),
                   "wss://app.dev/ws/dev-agent/producer?repoId=01REPO&workItem=12&runId=01RUN");
        assert_eq!(producer_url("http://localhost:3001/agent-api", &work()).unwrap(),
                   "ws://localhost:3001/ws/dev-agent/producer?repoId=01REPO&workItem=12&runId=01RUN");
        assert!(producer_url("ftp://x", &work()).is_none());
        // live_url explícito (Gateway em outro host).
        assert_eq!(producer_url("wss://gw.interno", &work()).unwrap(),
                   "wss://gw.interno/ws/dev-agent/producer?repoId=01REPO&workItem=12&runId=01RUN");
    }

    #[test]
    fn normaliza_como_o_session_runner() {
        let a = normalize(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"oi"}]},"session_id":"s"}"#);
        assert_eq!(a, vec![Frame { kind: "session.message", payload: json!({
            "msg_type": "assistant", "payload": { "message": { "content": [{ "type": "text", "text": "oi" }] } } }) }]);

        let init = normalize(r#"{"type":"system","subtype":"init","model":"claude-x","tools":["Bash"]}"#);
        assert_eq!(init[0].payload["payload"]["model"], "claude-x");

        // user só com tool_result; prompt comum não trafega.
        assert_eq!(normalize(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#).len(), 1);
        assert!(normalize(r#"{"type":"user","message":{"content":"faça X"}}"#).is_empty());

        // result: total_cost_usd vira cost_usd, e sai como session.result.
        let r = normalize(r#"{"type":"result","subtype":"success","duration_ms":10,"num_turns":3,"total_cost_usd":0.5,"is_error":false}"#);
        assert_eq!(r[0].kind, "session.result");
        assert_eq!(r[0].payload["cost_usd"], 0.5);

        let d = normalize(r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"parc"}}}"#);
        assert_eq!(d, vec![Frame { kind: "session.delta", payload: json!({ "text": "parc" }) }]);
    }

    #[test]
    fn linha_de_story_leva_a_origem_e_nao_encerra_a_sessao() {
        let story = |line: &str| LiveLine { origin: Some("story:92".into()), line: line.into() };
        let m = normalize_line(&story(r#"{"type":"assistant","message":{"content":[]}}"#));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].payload["payload"]["origin"], "story:92");
        // result e delta de uma story não viram frames (não são o fim da sessão).
        assert!(normalize_line(&story(r#"{"type":"result","subtype":"success"}"#)).is_empty());
        assert!(normalize_line(&story(r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"x"}}}"#)).is_empty());
        // Do orquestrador (sem origem), nada muda.
        let orq = LiveLine { origin: None, line: r#"{"type":"result","subtype":"success"}"#.into() };
        assert_eq!(normalize_line(&orq)[0].kind, "session.result");
    }

    #[test]
    fn acompanha_so_linhas_completas_de_cada_story() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut offsets = HashMap::new();
        std::fs::write(dir.path().join("story-92.jsonl"), "{\"a\":1}\n{\"b\"").unwrap();
        std::fs::write(dir.path().join("outro.txt"), "ignorar\n").unwrap();
        scan_stories(dir.path(), &mut offsets, &tx);
        let first = rx.try_recv().unwrap();
        assert_eq!(first, LiveLine { origin: Some("story:92".into()), line: "{\"a\":1}".into() });
        assert!(rx.try_recv().is_err(), "linha incompleta espera a próxima passada");
        // A linha termina de ser escrita: sai na próxima passada, sem repetir a primeira.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join("story-92.jsonl")).unwrap();
        f.write_all(b":2}\n").unwrap();
        scan_stories(dir.path(), &mut offsets, &tx);
        assert_eq!(rx.try_recv().unwrap().line, "{\"b\":2}");
        assert!(rx.try_recv().is_err());
        // Byte inválido não trava o arquivo: sai com substituição e segue.
        f.write_all(b"{\"c\":\"\xff\"}\n{\"d\":1}\n").unwrap();
        scan_stories(dir.path(), &mut offsets, &tx);
        assert_eq!(rx.try_recv().unwrap().line, "{\"c\":\"\u{fffd}\"}");
        assert_eq!(rx.try_recv().unwrap().line, "{\"d\":1}");
    }

    #[test]
    fn descarta_o_que_o_console_nao_conhece() {
        assert!(normalize(r#"{"type":"system","subtype":"hook_response"}"#).is_empty());
        assert!(normalize(r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(normalize("npx spec-wave implement 12").is_empty());
    }
}
