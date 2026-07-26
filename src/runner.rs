//! Workspace por issue + execução do `npx spec-wave implement` com
//! streaming do output do filho para o console do agente.

use crate::config::Config;
use crate::shell::{retry_backoff, run, run_ok};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{timeout, Duration as TokioDuration};
use tracing::info;

pub async fn ensure_workspace(cfg: &Config, issue: u64) -> Result<PathBuf> {
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

    // Retomada: se o branch de trabalho já existe no remoto (outro agente
    // começou e caiu), continua dele; senão cria a partir do default.
    let branch = format!("agent/issue-{issue}");
    let remote = run(&ws, "git",
                     &["ls-remote", "--exit-code", "origin",
                       &format!("refs/heads/{branch}")]).await?;
    if remote.ok {
        run_ok(&ws, "git", &["checkout", "-B", &branch,
                             &format!("origin/{branch}")]).await?;
        info!(target: "ws", "retomando branch existente {branch}");
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
pub async fn checkpoint(ws: &Path, issue: u64, label: &str) {
    let _ = run(ws, "git", &["add", "-A"]).await;
    let _ = run(ws, "git",
                &["commit", "-m",
                  &format!("wip: checkpoint #{issue} ({label}) [spec-wave-agent]")])
        .await; // pode falhar se não houver mudanças: ok
    let _ = run(ws, "git", &["push", "--quiet"]).await;
}

pub enum RunEnd { Success, Failed(String), LeaseLost, Interrupted }

/// Marker que o executor escreve ao final (ver feature_prompt): exit 0 sem
/// marker "ok" NÃO é sucesso — pega orquestrador que encerrou prematuramente
/// (ex: deixou implements em background).
const RESULT_MARKER: &str = ".spec-wave-agent-result.json";

#[derive(serde::Deserialize)]
struct ExecResult {
    status: String,
    #[serde(default)]
    detalhe: Option<String>,
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

async fn stream_lines<R>(reader: R, issue: u64, stream: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if stream == "out" {
            if let Some(msg) = render_out_line(&line) {
                info!(target: "spec-wave", "[#{issue}] {msg}");
            }
        } else {
            info!(target: "spec-wave", "[#{issue}][{stream}] {line}");
        }
    }
}

/// Mata o executor e TODA a sua árvore de processos (o executor spawna
/// `npx spec-wave` que spawna o claude interno; matar só o filho direto
/// deixaria netos órfãos trabalhando — buraco de fencing).
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // O filho é líder do grupo (process_group(0)); -pid = grupo inteiro.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
    }
    let _ = child.kill().await; // reap (e fallback não-unix)
}

/// Roda o executor da feature (Claude Code orquestrador): o comando vem de
/// `cfg.feature_command` e o prompt de orquestração entra via STDIN.
pub async fn run_feature_executor(
    cfg: &Config, ws: &Path, issue: u64,
    mut lease_lost: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<RunEnd> {
    let argv = crate::config::split_command(
        &crate::config::render_template(&cfg.feature_command, issue))?;
    let prompt = crate::config::render_template(&cfg.feature_prompt, issue);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0); // líder de grupo => kill_tree pega a árvore toda
    let mut child = cmd.spawn()
        .with_context(|| format!("falha ao iniciar executor {:?}", argv[0]))?;

    // Prompt via stdin (fechado em seguida — o executor lê até EOF).
    if let Some(mut si) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        si.write_all(prompt.as_bytes()).await
            .context("falha ao escrever o prompt no stdin do executor")?;
    }

    // Os handles saem do child via take(), então kill_tree nos braços
    // abaixo continua imediato — fencing não espera os readers.
    let out_task = child.stdout.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "out")));
    let err_task = child.stderr.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "err")));
    let group_pid = child.id(); // antes do wait (depois vira None)

    let cap = TokioDuration::from_secs(cfg.implement_timeout_secs);
    let end = tokio::select! {
        status = timeout(cap, child.wait()) => match status {
            Err(_) => {
                kill_tree(&mut child).await;
                RunEnd::Failed(format!("timeout de {}s", cap.as_secs()))
            }
            Ok(Ok(st)) if st.success() => RunEnd::Success,
            Ok(Ok(st)) => RunEnd::Failed(format!("exit code {st}")),
            Ok(Err(e)) => RunEnd::Failed(e.to_string()),
        },
        _ = lease_lost.changed() => {
            // Fencing: perdemos o lease => outro agente pode estar ativo.
            // Matar imediatamente, SEM push (o novo dono manda no branch).
            kill_tree(&mut child).await;
            RunEnd::LeaseLost
        }
        _ = shutdown.changed() => {
            kill_tree(&mut child).await;
            RunEnd::Interrupted
        }
    };

    // Mesmo em saída natural do executor, mata retardatários do grupo:
    // um orquestrador que "termina" deixando implements em background não
    // pode deixar claudes órfãos trabalhando.
    #[cfg(unix)]
    if let Some(pid) = group_pid {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
    }

    // Drena o resto do output (após kill/exit os pipes fecham em EOF).
    for t in [out_task, err_task].into_iter().flatten() {
        let _ = timeout(TokioDuration::from_secs(2), t).await;
    }

    // Exit 0 só vale como sucesso com o marker "ok" do executor.
    let end = match end {
        RunEnd::Success => match take_result_marker(ws) {
            Some(r) if r.status == "ok" => RunEnd::Success,
            Some(r) => RunEnd::Failed(format!(
                "executor terminou sem concluir tudo (status {:?}{})",
                r.status,
                r.detalhe.map(|d| format!(": {d}")).unwrap_or_default())),
            None => RunEnd::Failed(
                "executor saiu com exit 0 mas sem escrever o marker \
                 .spec-wave-agent-result.json (término prematuro? trabalho \
                 deixado em background?)".into()),
        },
        other => {
            let _ = take_result_marker(ws); // não deixar sujar o checkpoint
            other
        }
    };
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(line: &str) -> Option<String> {
        render_out_line(line)
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
