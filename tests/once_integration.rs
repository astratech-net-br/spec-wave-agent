//! `spec-wave-agent --once` (Fleet Job, RFC-008 fase 6): o binário de verdade
//! contra um bare repo LOCAL, com stubs de executor, `gh` e `npx` no PATH —
//! sem rede. Confere o contrato de eventos JSONL que o fleet-runner lê.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn sh(cwd: &Path, script: &str) {
    let st = Command::new("bash").args(["-ceu", script]).current_dir(cwd).status().expect("bash");
    assert!(st.success(), "script falhou: {script}");
}

fn exe(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Origin bare + stubs; devolve (config, PATH, FLEET_WORKDIR).
fn setup(tmp: &TempDir, executor_body: &str) -> (PathBuf, String, PathBuf) {
    let origin = tmp.path().join("origin.git");
    sh(tmp.path(), &format!(
        "git init -q --bare -b main {o}
         git init -q -b main seed && cd seed
         git config user.name t && git config user.email t@t
         echo base > README.md && git add -A && git commit -qm base
         git remote add origin {o} && git push -q origin main",
        o = origin.display()));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // gh: auth ok; pr list vazio; pr create devolve a URL.
    exe(&bin.join("gh"), r#"case "$1 $2" in
  "auth status") exit 0;;
  "pr list") exit 0;;
  "pr create") echo "https://github.com/test/test/pull/7";;
  "issue view") echo "Título da issue";;
esac
exit 0"#);
    exe(&bin.join("npx"), "echo 10.0.0");
    exe(&tmp.path().join("executor.sh"), executor_body);
    let cfg = tmp.path().join("agent.toml");
    std::fs::write(&cfg, format!(r#"
        remote_url = "{origin}"
        implement_timeout_secs = 60
        heartbeat_secs = 30
        lease_ttl_secs = 600
        feature_command = "{exec} {{issue}}"
    "#, origin = origin.display(), exec = tmp.path().join("executor.sh").display())).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let workdir = tmp.path().join("fleet");
    std::fs::create_dir_all(&workdir).unwrap();
    (cfg, path, workdir)
}

fn once(tmp: &TempDir, cfg: &Path, path: &str, workdir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_spec-wave-agent"));
    c.args(["--once", "--config", cfg.to_str().unwrap()])
        .current_dir(tmp.path())
        .env("PATH", path)
        .env("HOME", tmp.path())
        .env("GIT_AUTHOR_NAME", "fleet").env("GIT_AUTHOR_EMAIL", "f@f")
        .env("GIT_COMMITTER_NAME", "fleet").env("GIT_COMMITTER_EMAIL", "f@f")
        .env("SPECWAVE_HUB_REPO", "test/test")
        .env("SPECWAVE_WORK_ITEM", "42")
        .env("SPECWAVE_KIND", "feature")
        .env("RUN_ID", "01JAAAAAAAAAAAAAAAAAAAAAAA")
        .env("FLEET_WORKDIR", workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

fn events(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout).lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout não é JSONL ({e}): {l}")))
        .collect()
}

#[test]
fn once_emite_linhas_story_e_desfecho_com_pr() {
    let tmp = TempDir::new().unwrap();
    let (cfg, path, workdir) = setup(&tmp, r#"set -eu
cat > /dev/null
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"orquestrando"}]}}'
echo '{"type":"assistant","message":{"content":[]}}' > "$SPEC_WAVE_STREAM_DIR/story-3.jsonl"
echo trabalho > feito.txt && git add feito.txt && git commit -qm "story 3"
echo '{"status":"ok"}' > .spec-wave-agent-result.json
sleep 1"#);
    let out = once(&tmp, &cfg, &path, &workdir).output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    assert_eq!(evs.first().unwrap()["event"], "claimed");
    assert!(evs.iter().any(|e| e["event"] == "line" && e["origin"].is_null()
        && e["line"].as_str().unwrap().contains("orquestrando")), "linha do orquestrador: {evs:?}");
    assert!(evs.iter().any(|e| e["event"] == "line" && e["origin"] == "story:3"), "linha da story: {evs:?}");
    let last = evs.last().unwrap();
    assert_eq!(last["event"], "outcome");
    assert_eq!(last["state"], "succeeded");
    assert_eq!(last["pr_urls"][0], "https://github.com/test/test/pull/7");
    // O trabalho está no branch do remoto.
    let log = Command::new("git").args(["log", "--oneline", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("story 3"));
}

#[test]
fn once_sigterm_faz_checkpoint_e_termina_canceled() {
    let tmp = TempDir::new().unwrap();
    let (cfg, path, workdir) = setup(&tmp, r#"set -eu
cat > /dev/null
echo wip > meio.txt
sleep 30"#);
    let child = once(&tmp, &cfg, &path, &workdir).spawn().unwrap();
    let pid = child.id();
    // Espera o executor começar (o arquivo do WIP aparece no clone).
    let start = Instant::now();
    while !workdir.join("agent/issue-42/meio.txt").exists() {
        assert!(start.elapsed() < Duration::from_secs(20), "executor não começou");
        std::thread::sleep(Duration::from_millis(100));
    }
    Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap();
    let out = child.wait_with_output().unwrap();
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["state"], "canceled", "eventos: {evs:?}");
    // Checkpoint: o WIP foi para o branch.
    let files = Command::new("git").args(["ls-tree", "-r", "--name-only", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    assert!(String::from_utf8_lossy(&files.stdout).contains("meio.txt"), "WIP sem checkpoint");
}

#[test]
fn once_sem_ambiente_falha_com_desfecho() {
    let tmp = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_spec-wave-agent")).arg("--once")
        .env_remove("SPECWAVE_WORK_ITEM").env_remove("SPECWAVE_HUB_REPO")
        .current_dir(tmp.path()).stdout(Stdio::piped()).stderr(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    let evs = events(&out.stdout);
    assert_eq!(evs.last().unwrap()["state"], "failed");
}
