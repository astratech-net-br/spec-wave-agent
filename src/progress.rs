//! Progresso por Task de uma Feature (barra de progresso da tela Development).
//!
//! O agente não tem como saber, olhando o executor, que uma Task terminou —
//! quem sabe é a CLI. Duas fontes, as duas dela:
//!
//!   - o RETRATO: `npx spec-wave progress <feature> --json` (CLI ≥ 1.9) diz
//!     quantas Stories/Tasks a Feature tem e quais já estão concluídas — o
//!     que inclui o trabalho de uma execução anterior (retomada);
//!   - os EVENTOS: sob SPEC_WAVE_STREAM_DIR, o `spec-wave task done` e o fim de
//!     cada `spec-wave implement <story>` acrescentam uma linha JSON em
//!     `<stream_dir>/progress.jsonl`.
//!
//! O agente guarda CONJUNTOS (quais Tasks/Stories estão concluídas), não
//! contadores: evento repetido ou retrato relido não conta duas vezes, e nada
//! "desconclui". Tudo aqui é melhor esforço — CLI antiga, tracker fora do ar
//! ou arquivo ilegível só deixam a barra parada; a execução não depende disto.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// Arquivo de eventos que a CLI grava dentro do SPEC_WAVE_STREAM_DIR.
pub const PROGRESS_FILE: &str = "progress.jsonl";

/// Teto do retrato: uma listagem por Story no tracker; passou disso, a barra
/// fica só com os eventos.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// O que vai para o spec-wave (heartbeat da API e evento `progress` do
/// `--once`). `round` é a rodada do executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub round: u32,
    pub stories_done: u32,
    pub stories_total: u32,
    pub tasks_done: u32,
    pub tasks_total: u32,
}

#[derive(Deserialize)]
struct SnapshotItem {
    number: u64,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct SnapshotStory {
    number: u64,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    tasks: Vec<SnapshotItem>,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    stories: Vec<SnapshotStory>,
}

#[derive(Deserialize)]
struct Event {
    event: String,
    task: Option<u64>,
    story: Option<u64>,
    #[serde(default)]
    tasks: Vec<u64>,
}

/// Estado conhecido da Feature: número da issue → concluída.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    stories: BTreeMap<u64, bool>,
    tasks: BTreeMap<u64, bool>,
    /// Já houve um retrato com Stories: só ele dá o TOTAL. Sem ele os eventos
    /// conhecem apenas o que terminou, e a barra nasceria em 100%.
    snapshot: bool,
}

fn mark(map: &mut BTreeMap<u64, bool>, n: u64, done: bool) -> bool {
    match map.get(&n) {
        Some(true) => false,
        Some(false) if !done => false,
        _ => {
            map.insert(n, done);
            true
        }
    }
}

impl Tally {
    /// Aplica a saída do `spec-wave progress --json` (a última linha que for
    /// um objeto JSON — avisos do npx podem vir antes). Só acrescenta: o que
    /// um evento já marcou como concluído não volta atrás por um retrato
    /// tirado um instante antes. Devolve se algo mudou.
    pub fn apply_snapshot(&mut self, stdout: &str) -> bool {
        let Some(snap) = stdout.lines().rev()
            .filter(|l| l.trim_start().starts_with('{'))
            .find_map(|l| serde_json::from_str::<Snapshot>(l).ok()) else { return false };
        let mut changed = false;
        self.snapshot |= !snap.stories.is_empty();
        for s in snap.stories {
            changed |= mark(&mut self.stories, s.number, s.done);
            for t in s.tasks {
                changed |= mark(&mut self.tasks, t.number, t.done);
            }
        }
        changed
    }

    /// Aplica uma linha do `progress.jsonl`. Linha que não é evento conhecido
    /// é ignorada (a CLI pode ganhar eventos antes do agente).
    pub fn apply_event(&mut self, line: &str) -> bool {
        let Ok(ev) = serde_json::from_str::<Event>(line) else { return false };
        let mut changed = false;
        match ev.event.as_str() {
            "task_done" => {
                if let Some(t) = ev.task {
                    changed |= mark(&mut self.tasks, t, true);
                }
            }
            "story_done" => {
                if let Some(s) = ev.story {
                    changed |= mark(&mut self.stories, s, true);
                }
                for t in ev.tasks {
                    changed |= mark(&mut self.tasks, t, true);
                }
            }
            _ => {}
        }
        changed
    }

    /// `None` enquanto não se conhece Task nenhuma ou não houve retrato: barra
    /// sem denominador não é barra — só com eventos, o total seria o que já
    /// terminou (1/1 na primeira Task concluída).
    pub fn progress(&self, round: u32) -> Option<Progress> {
        if !self.snapshot || self.tasks.is_empty() {
            return None;
        }
        let done = |m: &BTreeMap<u64, bool>| m.values().filter(|d| **d).count() as u32;
        Some(Progress {
            round,
            stories_done: done(&self.stories),
            stories_total: self.stories.len() as u32,
            tasks_done: done(&self.tasks),
            tasks_total: self.tasks.len() as u32,
        })
    }
}

/// Lê as linhas COMPLETAS novas do arquivo de eventos (a partir de `offset`).
/// Linha sem `\n` ainda está sendo escrita — fica para a próxima passada.
fn read_new_events(file: &Path, offset: &mut u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(file) else { return vec![] };
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return vec![] };
    *offset += (end + 1) as u64;
    String::from_utf8_lossy(&buf[..end]).lines().map(str::to_string).collect()
}

/// Roda o comando do retrato no workspace. `None` = sem retrato (CLI antiga,
/// tracker fora do ar, tempo esgotado).
async fn snapshot(argv: &[String], ws: &Path) -> Option<String> {
    let (bin, args) = argv.split_first()?;
    let run = tokio::process::Command::new(bin)
        .args(args)
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, run).await {
        Ok(Ok(out)) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Ok(out)) => {
            debug!(target: "agent", "retrato de progresso indisponível (exit {:?})", out.status.code());
            None
        }
        Ok(Err(e)) => {
            debug!(target: "agent", "retrato de progresso indisponível: {e}");
            None
        }
        Err(_) => {
            debug!(target: "agent", "retrato de progresso: tempo esgotado");
            None
        }
    }
}

/// Acompanha o progresso de UMA execução e publica cada mudança em `out`.
pub struct ProgressWatch {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ProgressWatch {
    /// `command`: argv do retrato, já com a issue. `round`: a rodada corrente
    /// do executor (`LiveHooks::round`) — a cada rodada nova o retrato é
    /// relido, porque o executor recomeçou e o tracker é a verdade. Retrato
    /// que falhou é tentado de novo quando chega um evento (uma tentativa por
    /// leva de eventos): sem ele não há total, logo não há barra.
    pub fn spawn(
        command: Vec<String>, ws: PathBuf, stream_dir: PathBuf,
        round: Arc<AtomicU32>, out: Arc<watch::Sender<Option<Progress>>>,
    ) -> ProgressWatch {
        let (stop, mut stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let file = stream_dir.join(PROGRESS_FILE);
            let mut tally = Tally::default();
            let mut offset = 0u64;
            let mut seen_round = 0u32;
            let mut retry_snapshot = false;
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            // O retrato pode levar minutos: os ticks perdidos não são repostos
            // em rajada.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let publish = |tally: &Tally, round: u32| {
                let Some(p) = tally.progress(round.max(1)) else { return };
                let modified = out.send_if_modified(|cur| {
                    if *cur == Some(p) {
                        return false;
                    }
                    *cur = Some(p);
                    true
                });
                if modified {
                    debug!(target: "agent", "progresso: tasks {}/{} · stories {}/{}",
                           p.tasks_done, p.tasks_total, p.stories_done, p.stories_total);
                }
            };
            loop {
                let current = round.load(Ordering::SeqCst);
                if seen_round == 0 || current > seen_round || retry_snapshot {
                    seen_round = current.max(1);
                    tokio::select! {
                        snap = snapshot(&command, &ws) => {
                            if let Some(s) = snap {
                                tally.apply_snapshot(&s);
                                if let Some(p) = tally.progress(seen_round) {
                                    info!(target: "agent", "progresso da feature: tasks {}/{}",
                                          p.tasks_done, p.tasks_total);
                                }
                            }
                        }
                        _ = stop_rx.changed() => {}
                    }
                }
                let events = read_new_events(&file, &mut offset);
                retry_snapshot = !tally.snapshot && !events.is_empty();
                for line in events {
                    tally.apply_event(&line);
                }
                publish(&tally, seen_round);
                if *stop_rx.borrow() {
                    return;
                }
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stop_rx.changed() => {
                        // Passada final: o último `story_done` chega junto com
                        // o fim do executor.
                        for line in read_new_events(&file, &mut offset) {
                            tally.apply_event(&line);
                        }
                        publish(&tally, seen_round);
                        return;
                    }
                }
            }
        });
        ProgressWatch { stop, task }
    }

    pub async fn finish(self) {
        let _ = self.stop.send(true);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(3), &mut task).await.is_err() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAP: &str = r#"npm warn qualquer
{"feature":12,"storiesDone":1,"storiesTotal":2,"tasksDone":2,"tasksTotal":5,"stories":[{"number":34,"done":true,"tasks":[{"number":40,"done":true},{"number":41,"done":true}]},{"number":35,"done":false,"tasks":[{"number":42,"done":false},{"number":43,"done":false},{"number":44,"done":false}]}]}
"#;

    #[test]
    fn retrato_da_o_total_e_o_que_ja_estava_pronto() {
        let mut t = Tally::default();
        assert!(t.apply_snapshot(SNAP));
        assert_eq!(t.progress(1), Some(Progress {
            round: 1, stories_done: 1, stories_total: 2, tasks_done: 2, tasks_total: 5 }));
    }

    #[test]
    fn sem_task_conhecida_nao_ha_progresso() {
        let mut t = Tally::default();
        assert!(!t.apply_snapshot("error: unknown command 'progress'"));
        assert!(!t.apply_snapshot(r#"{"feature":12,"stories":[]}"#));
        assert_eq!(t.progress(1), None);
    }

    #[test]
    fn evento_de_task_avanca_uma_e_repetido_nao_conta_de_novo() {
        let mut t = Tally::default();
        t.apply_snapshot(SNAP);
        assert!(t.apply_event(r#"{"event":"task_done","task":42}"#));
        assert!(!t.apply_event(r#"{"event":"task_done","task":42}"#));
        assert_eq!(t.progress(1).unwrap().tasks_done, 3);
    }

    #[test]
    fn fim_da_story_conclui_as_tasks_que_ninguem_sinalizou() {
        let mut t = Tally::default();
        t.apply_snapshot(SNAP);
        t.apply_event(r#"{"event":"task_done","task":42}"#);
        assert!(t.apply_event(r#"{"event":"story_done","story":35,"tasks":[42,43,44]}"#));
        let p = t.progress(2).unwrap();
        assert_eq!((p.tasks_done, p.tasks_total, p.stories_done, p.stories_total, p.round), (5, 5, 2, 2, 2));
    }

    #[test]
    fn retrato_relido_nao_desfaz_o_que_um_evento_ja_concluiu() {
        let mut t = Tally::default();
        t.apply_event(r#"{"event":"task_done","task":42}"#);
        t.apply_snapshot(SNAP); // tirado antes: ainda diz 42 pendente
        assert_eq!(t.progress(1).unwrap().tasks_done, 3);
    }

    #[test]
    fn so_com_eventos_nao_ha_barra_ate_o_retrato_dar_o_total() {
        let mut t = Tally::default();
        assert!(t.apply_event(r#"{"event":"task_done","task":42}"#));
        assert_eq!(t.progress(1), None, "1/1 seria uma barra em 100% na primeira Task");
        t.apply_snapshot(SNAP);
        assert_eq!(t.progress(1).unwrap().tasks_total, 5);
    }

    #[test]
    fn linha_desconhecida_ou_invalida_e_ignorada() {
        let mut t = Tally::default();
        assert!(!t.apply_event("não é json"));
        assert!(!t.apply_event(r#"{"event":"novidade","task":1}"#));
        assert!(!t.apply_event(r#"{"event":"task_done"}"#));
        assert_eq!(t, Tally::default());
    }

    #[test]
    fn le_so_linhas_completas_do_arquivo_de_eventos() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(PROGRESS_FILE);
        let mut offset = 0u64;
        assert!(read_new_events(&file, &mut offset).is_empty()); // ainda não existe
        std::fs::write(&file, "{\"event\":\"task_done\",\"task\":1}\n{\"event\":\"task_d").unwrap();
        assert_eq!(read_new_events(&file, &mut offset), vec![r#"{"event":"task_done","task":1}"#.to_string()]);
        assert!(read_new_events(&file, &mut offset).is_empty());
        std::fs::write(&file, "{\"event\":\"task_done\",\"task\":1}\n{\"event\":\"task_done\",\"task\":2}\n").unwrap();
        assert_eq!(read_new_events(&file, &mut offset), vec![r#"{"event":"task_done","task":2}"#.to_string()]);
    }

    #[test]
    fn serializa_em_camel_case_para_a_api() {
        let p = Progress { round: 1, stories_done: 0, stories_total: 2, tasks_done: 3, tasks_total: 7 };
        assert_eq!(serde_json::to_value(p).unwrap(), serde_json::json!({
            "round": 1, "storiesDone": 0, "storiesTotal": 2, "tasksDone": 3, "tasksTotal": 7 }));
    }
}
