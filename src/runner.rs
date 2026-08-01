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

#[derive(Debug)]
pub enum RunEnd { Success(Option<ExecResult>), Failed(String), LeaseLost, Interrupted }

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
enum RoundEnd { Success(Option<ExecResult>), Incomplete, Failed(String), LeaseLost, Interrupted }

/// Roda UMA rodada do executor da feature (Claude Code orquestrador): o
/// comando vem de `cfg.feature_command` e o prompt entra via STDIN.
async fn executor_round(
    cfg: &Config, ws: &Path, item: QueueItem,
    mut lease_lost: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<RoundEnd> {
    let issue = item.number;
    let (command, prompt_tpl, timeout_secs) = match item.kind {
        QueueKind::Bug => (&cfg.bug_command, &cfg.bug_prompt, cfg.bug_timeout_secs),
        QueueKind::Feature =>
            (&cfg.feature_command, &cfg.feature_prompt, cfg.implement_timeout_secs),
    };
    let argv = crate::config::split_command(
        &crate::config::render_template(command, issue))?;
    let prompt = crate::config::render_template(prompt_tpl, issue);

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
        .map(|s| tokio::spawn(stream_lines(s, issue, "out")));
    let err_task = child.stderr.take()
        .map(|s| tokio::spawn(stream_lines(s, issue, "err")));

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

    // Exit 0 só vale como sucesso com o marker "ok" do executor; sem
    // marker é "rodada incompleta" (o run_feature relança para continuar).
    let end = match end {
        RoundRaw::Success => match take_result_marker(ws) {
            Some(r) if r.status == "ok" => RoundEnd::Success(Some(r)),
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
    let issue = item.number;
    let mut no_progress = 0u32;
    for round in 1..=cfg.max_executor_rounds {
        let head_before = head_sha(ws).await;
        let end = executor_round(cfg, ws, item,
                                 lease_lost.clone(), shutdown.clone()).await?;
        match end {
            RoundEnd::Success(r) => return Ok(RunEnd::Success(r)),
            RoundEnd::Failed(r) => return Ok(RunEnd::Failed(r)),
            RoundEnd::LeaseLost => return Ok(RunEnd::LeaseLost),
            RoundEnd::Interrupted => return Ok(RunEnd::Interrupted),
            RoundEnd::Incomplete => {
                let progressed = head_sha(ws).await != head_before;
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
