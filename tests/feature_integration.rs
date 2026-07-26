//! Integração do executor de feature contra um bare repo LOCAL e um stub
//! no lugar do Claude Code — sem rede, sem gh, sem npm.

use spec_wave_agent::config::Config;
use spec_wave_agent::runner::{ensure_workspace, run_feature, RunEnd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use tokio::sync::watch;

fn sh(cwd: &Path, script: &str) {
    let st = Command::new("bash").args(["-ceu", script]).current_dir(cwd)
        .status().expect("bash");
    assert!(st.success(), "script falhou: {script}");
}

/// Origin bare com um commit inicial em main + Config apontando para ele.
fn setup(tmp: &TempDir, stub_body: &str, timeout_secs: u64) -> Config {
    let origin = tmp.path().join("origin.git");
    let seed = tmp.path().join("seed");
    sh(tmp.path(), &format!(
        "git init -q --bare -b main {o}
         git init -q -b main seed
         cd seed
         git config user.name t; git config user.email t@t
         echo base > README.md
         git add -A && git commit -qm base
         git remote add origin {o}
         git push -q origin main",
        o = origin.display()));
    drop(seed);

    let stub = tmp.path().join("stub.sh");
    std::fs::write(&stub, format!("#!/usr/bin/env bash\nset -eu\n{stub_body}\n")).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let toml_str = format!(r#"
        repo = "test/test"
        remote_url = "{origin}"
        workdir = "{workdir}"
        implement_timeout_secs = {timeout_secs}
        feature_command = "{stub} {{issue}}"
        feature_prompt = "orquestre a feature {{issue}} com spec-wave order {{issue}}"
    "#, origin = origin.display(), workdir = tmp.path().join("wd").display(),
        stub = stub.display());
    toml::from_str(&toml_str).unwrap()
}

fn channels() -> (watch::Sender<bool>, watch::Receiver<bool>,
                  watch::Sender<bool>, watch::Receiver<bool>) {
    let (lt, lr) = watch::channel(false);
    let (st, sr) = watch::channel(false);
    (lt, lr, st, sr)
}

#[tokio::test]
async fn sucesso_recebe_prompt_e_pusha_trabalho() {
    let tmp = TempDir::new().unwrap();
    // O stub grava o prompt (stdin), simula trabalho e pusha no branch atual.
    let cfg = setup(&tmp, r#"
        cat > prompt-recebido.txt
        echo "implementando feature $1"
        git add -A && git config user.name t && git config user.email t@t
        git commit -qm "feat: story #999 [spec-wave-agent]"
        git push -q
        printf '{"status": "ok"}' > .spec-wave-agent-result.json
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 7).await.unwrap();
    let end = run_feature(&cfg, &ws, 7, lr, sr).await.unwrap();
    assert!(matches!(end, RunEnd::Success), "esperava Success");

    // Prompt chegou pelo stdin com o placeholder renderizado.
    let prompt = std::fs::read_to_string(ws.join("prompt-recebido.txt")).unwrap();
    assert!(prompt.contains("spec-wave order 7"), "prompt: {prompt}");

    // Trabalho da story está no origin, no branch da feature.
    let out = Command::new("git")
        .args(["log", "--format=%s", "agent/issue-7"])
        .current_dir(tmp.path().join("origin.git"))
        .output().unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("feat: story #999 [spec-wave-agent]"), "log: {log}");

    // O agente consome o marker (não pode sobrar para o checkpoint).
    assert!(!ws.join(".spec-wave-agent-result.json").exists());
}

#[tokio::test]
async fn exit_zero_sem_marker_nao_e_sucesso() {
    let tmp = TempDir::new().unwrap();
    // Simula o orquestrador que "termina" cedo demais TODA rodada: exit 0,
    // sem marker e sem commits => 2 rodadas sem progresso => falha.
    let cfg = setup(&tmp, "cat > /dev/null\necho rodada >> rodadas.txt\nexit 0", 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 10).await.unwrap();
    let end = run_feature(&cfg, &ws, 10, lr, sr).await.unwrap();
    match end {
        RunEnd::Failed(r) => assert!(r.contains("sem progresso"), "motivo: {r}"),
        _ => panic!("exit 0 sem marker e sem progresso deveria ser Failed"),
    }
    // Foram exatamente 2 rodadas (estagnação detectada na 2ª).
    let rodadas = std::fs::read_to_string(ws.join("rodadas.txt")).unwrap();
    assert_eq!(rodadas.lines().count(), 2);
}

#[tokio::test]
async fn rodada_incompleta_com_progresso_e_relancada_ate_o_marker() {
    let tmp = TempDir::new().unwrap();
    // Rodada 1: commita uma story e sai SEM marker (turno encerrado cedo).
    // Rodada 2: percebe o estado, conclui e escreve o marker ok.
    let cfg = setup(&tmp, r#"
        cat > /dev/null
        git config user.name t; git config user.email t@t
        if [ ! -f fase2 ]; then
            touch fase2 story-a.txt
            git add story-a.txt && git commit -qm "feat: story #1 [spec-wave-agent]"
            git push -q
            exit 0
        fi
        touch story-b.txt
        git add -A && git commit -qm "feat: story #2 [spec-wave-agent]" && git push -q
        printf '{"status": "ok"}' > .spec-wave-agent-result.json
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 12).await.unwrap();
    let end = run_feature(&cfg, &ws, 12, lr, sr).await.unwrap();
    assert!(matches!(end, RunEnd::Success), "esperava Success após 2 rodadas");

    let out = std::process::Command::new("git")
        .args(["log", "--format=%s", "agent/issue-12"])
        .current_dir(tmp.path().join("origin.git"))
        .output().unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("story #1") && log.contains("story #2"), "log: {log}");
}

#[tokio::test]
async fn marker_partial_vira_falha_com_detalhe() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, r#"
        cat > /dev/null
        printf '{"status": "partial", "detalhe": "story #215 falhou nos testes"}' \
            > .spec-wave-agent-result.json
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 11).await.unwrap();
    let end = run_feature(&cfg, &ws, 11, lr, sr).await.unwrap();
    match end {
        RunEnd::Failed(r) => {
            assert!(r.contains("partial") && r.contains("story #215"), "motivo: {r}");
        }
        _ => panic!("status partial deveria ser Failed"),
    }
    assert!(!ws.join(".spec-wave-agent-result.json").exists());
}

#[tokio::test]
async fn timeout_mata_a_arvore_de_processos_inteira() {
    let tmp = TempDir::new().unwrap();
    // O stub spawna um NETO (sleep) e trava: simula npx -> claude interno.
    let cfg = setup(&tmp, r#"
        sleep 300 &
        echo $! > neto.pid
        sleep 300
    "#, 2);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 8).await.unwrap();
    let end = run_feature(&cfg, &ws, 8, lr, sr).await.unwrap();
    match end {
        RunEnd::Failed(r) => assert!(r.contains("timeout"), "motivo: {r}"),
        _ => panic!("esperava Failed(timeout)"),
    }

    // O neto tem que morrer junto (kill de grupo). kill -0 falha se morto.
    let pid: i32 = std::fs::read_to_string(ws.join("neto.pid")).unwrap()
        .trim().parse().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "neto (pid {pid}) sobreviveu ao kill de grupo");
}

#[tokio::test]
async fn executor_com_exit_1_falha() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, "cat > /dev/null\nexit 1", 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 9).await.unwrap();
    let end = run_feature(&cfg, &ws, 9, lr, sr).await.unwrap();
    assert!(matches!(end, RunEnd::Failed(_)), "esperava Failed");
}
