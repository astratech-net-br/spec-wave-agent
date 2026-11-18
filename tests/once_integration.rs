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
    setup_with(tmp, executor_body, "echo 10.0.0", true)
}

/// `npx_body`: o stub do npx (a CLI). `remote_in_config`: false tira o
/// remote_url da config — o teste passa SPECWAVE_REMOTE_URL pelo ambiente.
fn setup_with(tmp: &TempDir, executor_body: &str, npx_body: &str, remote_in_config: bool)
    -> (PathBuf, String, PathBuf)
{
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
    exe(&bin.join("npx"), npx_body);
    exe(&tmp.path().join("executor.sh"), executor_body);
    let cfg = tmp.path().join("agent.toml");
    let remote = if remote_in_config { format!("remote_url = \"{}\"", origin.display()) } else { String::new() };
    std::fs::write(&cfg, format!(r#"
        {remote}
        implement_timeout_secs = 60
        ci_poll_secs = 1
        ci_no_checks_grace_secs = 1
        heartbeat_secs = 30
        lease_ttl_secs = 600
        feature_command = "{exec} {{issue}}"
    "#, exec = tmp.path().join("executor.sh").display())).unwrap();
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

#[test]
fn once_abre_o_pr_pela_cli_e_clona_pela_url_do_ambiente() {
    let tmp = TempDir::new().unwrap();
    let args_log = tmp.path().join("npx-args.txt");
    // A CLI (1.7.0+) abre o PR; o gh, se fosse chamado para criar, falharia.
    let npx = format!(r#"if [ "$1 $2 $3" = "spec-wave pr open" ]; then
  printf '%s\n' "$@" > {log}
  echo '{{"url":"https://scm.example/test/test/pull/9","number":"9","created":true}}'
  exit 0
fi
echo 10.0.0"#, log = args_log.display());
    let (cfg, path, workdir) = setup_with(&tmp, r#"set -eu
cat > /dev/null
echo trabalho > feito.txt && git add feito.txt && git commit -qm "story 1"
echo '{"status":"ok"}' > .spec-wave-agent-result.json"#, &npx, false);
    std::fs::write(tmp.path().join("bin/gh"), "#!/usr/bin/env bash\n[ \"$1 $2\" = \"auth status\" ] && exit 0\nexit 1\n").unwrap();
    let origin = tmp.path().join("origin.git");
    let out = once(&tmp, &cfg, &path, &workdir)
        .env("SPECWAVE_REMOTE_URL", origin.to_str().unwrap())
        .output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["state"], "succeeded", "eventos: {evs:?}");
    assert_eq!(last["pr_urls"][0], "https://scm.example/test/test/pull/9");
    let args = std::fs::read_to_string(&args_log).unwrap();
    let args: Vec<&str> = args.lines().collect();
    for pair in [["--repo", "test/test"], ["--head", "agent/issue-42"], ["--work-item", "42"], ["--hub", "test/test"]] {
        let i = args.iter().position(|a| *a == pair[0]).unwrap_or_else(|| panic!("sem {}: {args:?}", pair[0]));
        assert_eq!(args[i + 1], pair[1]);
    }
    assert!(args.contains(&"--json"));
    // O clone veio da URL do ambiente: o trabalho está no remoto.
    let log = Command::new("git").args(["log", "--oneline", "agent/issue-42"])
        .current_dir(&origin).output().unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("story 1"));
}

#[test]
fn once_emite_progresso_a_cada_task_concluida() {
    let tmp = TempDir::new().unwrap();
    // A CLI: o retrato diz 3 Tasks, 1 já pronta de uma execução anterior.
    let npx = r#"if [ "$1 $2" = "spec-wave progress" ]; then
  echo 'npm warn exec qualquer'
  echo '{"feature":42,"stories":[{"number":3,"done":false,"tasks":[{"number":5,"done":true},{"number":6,"done":false},{"number":7,"done":false}]}]}'
  exit 0
fi
echo 10.0.0"#;
    // O executor: espera o retrato, conclui a Task 6 (`task done`) e depois a
    // Story inteira (fim do `implement`).
    let (cfg, path, workdir) = setup_with(&tmp, r#"set -eu
cat > /dev/null
sleep 1.5
echo '{"event":"task_done","task":6}' >> "$SPEC_WAVE_STREAM_DIR/progress.jsonl"
sleep 1.5
echo '{"event":"story_done","story":3,"tasks":[5,6,7]}' >> "$SPEC_WAVE_STREAM_DIR/progress.jsonl"
echo trabalho > feito.txt && git add feito.txt && git commit -qm "story 3"
echo '{"status":"ok"}' > .spec-wave-agent-result.json"#, npx, true);
    let out = once(&tmp, &cfg, &path, &workdir).output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    let progresso: Vec<(u64, u64, u64, u64)> = evs.iter().filter(|e| e["event"] == "progress")
        .map(|e| (e["tasks_done"].as_u64().unwrap(), e["tasks_total"].as_u64().unwrap(),
                  e["stories_done"].as_u64().unwrap(), e["stories_total"].as_u64().unwrap()))
        .collect();
    assert_eq!(progresso, vec![(1, 3, 0, 1), (2, 3, 0, 1), (3, 3, 1, 1)], "eventos: {evs:?}");
    assert!(evs.iter().filter(|e| e["event"] == "progress").all(|e| e["round"] == 1));
    // O último progresso sai antes do desfecho, que continua sendo a última linha.
    assert_eq!(evs.last().unwrap()["event"], "outcome");
    assert_eq!(evs.last().unwrap()["state"], "succeeded");
}

#[test]
fn once_com_cli_sem_o_comando_de_progresso_segue_sem_barra() {
    let tmp = TempDir::new().unwrap();
    // npx padrão ("echo 10.0.0"): não devolve retrato. Sem retrato não há
    // total, logo não há barra — nem que cheguem eventos.
    let (cfg, path, workdir) = setup(&tmp, r#"set -eu
cat > /dev/null
echo trabalho > feito.txt && git add feito.txt && git commit -qm "story 3"
echo '{"status":"ok"}' > .spec-wave-agent-result.json"#);
    let out = once(&tmp, &cfg, &path, &workdir).output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    assert!(evs.iter().all(|e| e["event"] != "progress"), "sem Task conhecida não há evento: {evs:?}");
    assert_eq!(evs.last().unwrap()["state"], "succeeded");
}

/// Setup da revisão de PR: o executor entra pelo `review_command`, e o stub do
/// `gh` REGISTRA cada chamada — a revisão não pode abrir PR.
fn setup_review(tmp: &TempDir, executor_body: &str) -> (PathBuf, String, PathBuf) {
    let (cfg, path, workdir) = setup(tmp, "echo 'feature_command não deveria rodar' >&2; exit 1");
    exe(&tmp.path().join("review.sh"), executor_body);
    exe(&tmp.path().join("bin/gh"), &format!(r#"echo "$*" >> {log}
case "$1 $2" in
  "pr create") echo "https://github.com/test/test/pull/99";;
esac
exit 0"#, log = tmp.path().join("gh.log").display()));
    let mut toml = std::fs::read_to_string(&cfg).unwrap();
    toml.push_str(&format!("\n        review_command = \"{} {{issue}}\"\n",
                           tmp.path().join("review.sh").display()));
    std::fs::write(&cfg, toml).unwrap();
    (cfg, path, workdir)
}

#[test]
fn once_review_reporta_os_prs_revisados_sem_abrir_pr() {
    let tmp = TempDir::new().unwrap();
    // O executor recebe o prompt de REVISÃO pelo stdin, commita uma correção e
    // declara no marker o PR que revisou.
    let (cfg, path, workdir) = setup_review(&tmp, &format!(r#"set -eu
cat > {prompt}
echo corrigido > fix.txt && git add fix.txt && git commit -qm "fix: achado do review (#42)"
echo '{{"status":"ok","prs":["https://github.com/test/test/pull/7"],"detalhe":"1 achado corrigido"}}' > .spec-wave-agent-result.json"#,
        prompt = tmp.path().join("prompt.txt").display()));
    let out = once(&tmp, &cfg, &path, &workdir).env("SPECWAVE_KIND", "review").output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["event"], "outcome");
    assert_eq!(last["state"], "succeeded", "eventos: {evs:?}");
    assert_eq!(last["pr_urls"], serde_json::json!(["https://github.com/test/test/pull/7"]));

    // O prompt é o de revisão, com a issue renderizada.
    let prompt = std::fs::read_to_string(tmp.path().join("prompt.txt")).unwrap();
    assert!(prompt.contains("Feature #42"), "prompt: {prompt}");
    assert!(prompt.contains("spec-wave:auto-review"));
    assert!(prompt.contains("NUNCA aprove"));

    // A correção foi pushada para o branch de trabalho.
    let log = Command::new("git").args(["log", "--oneline", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("achado do review"));

    // Revisão NÃO abre PR nem consulta checks: o `gh` do agente não é chamado
    // para isso (o executor de verdade usa o gh por conta própria).
    let gh = std::fs::read_to_string(tmp.path().join("gh.log")).unwrap_or_default();
    assert!(!gh.contains("pr create"), "revisão abriu PR: {gh}");
    assert!(!gh.contains("pr view"), "revisão esperou CI: {gh}");
}

#[test]
fn once_review_sem_pr_elegivel_termina_blocked() {
    let tmp = TempDir::new().unwrap();
    let (cfg, path, workdir) = setup_review(&tmp, r#"set -eu
cat > /dev/null
echo '{"status":"ok","prs":[]}' > .spec-wave-agent-result.json"#);
    let out = once(&tmp, &cfg, &path, &workdir).env("SPECWAVE_KIND", "review").output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status);
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["state"], "blocked", "eventos: {evs:?}");
    assert!(last["reason"].as_str().unwrap().contains("nenhum Pull Request"));
    assert_eq!(last["pr_urls"], serde_json::json!([]));
}

#[test]
fn once_kind_desconhecido_continua_recusado() {
    let tmp = TempDir::new().unwrap();
    let (cfg, path, workdir) = setup(&tmp, "exit 0");
    let out = once(&tmp, &cfg, &path, &workdir).env("SPECWAVE_KIND", "deploy").output().unwrap();
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["state"], "failed");
    assert!(last["reason"].as_str().unwrap().contains("SPECWAVE_KIND desconhecido"));
}

#[test]
fn once_review_que_falha_nao_pusha_trabalho_pela_metade() {
    let tmp = TempDir::new().unwrap();
    // O executor deixa uma edição sem commit e termina com status de falha: a
    // revisão é uma rodada só, e esse resto não pode virar um commit "wip" na
    // branch do PR.
    let (cfg, path, workdir) = setup_review(&tmp, r#"set -eu
cat > /dev/null
echo metade > pela-metade.txt
echo '{"status":"erro","detalhe":"suíte quebrou"}' > .spec-wave-agent-result.json"#);
    let out = once(&tmp, &cfg, &path, &workdir).env("SPECWAVE_KIND", "review").output().unwrap();
    let evs = events(&out.stdout);
    let last = evs.last().unwrap();
    assert_eq!(last["state"], "failed", "eventos: {evs:?}");
    let files = Command::new("git").args(["ls-tree", "-r", "--name-only", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    assert!(!String::from_utf8_lossy(&files.stdout).contains("pela-metade.txt"),
            "revisão que falhou pushou WIP");
    let log = Command::new("git").args(["log", "--oneline", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    assert!(!String::from_utf8_lossy(&log.stdout).contains("checkpoint"));
}

#[test]
fn once_review_sucesso_pusha_commits_mas_nao_sobras() {
    let tmp = TempDir::new().unwrap();
    let (cfg, path, workdir) = setup_review(&tmp, r#"set -eu
cat > /dev/null
echo corrigido > fix.txt && git add fix.txt && git commit -qm "fix: achado (#42)"
echo sobra > sem-commit.txt
echo '{"status":"ok","prs":["https://github.com/test/test/pull/7"]}' > .spec-wave-agent-result.json"#);
    let out = once(&tmp, &cfg, &path, &workdir).env("SPECWAVE_KIND", "review").output().unwrap();
    let evs = events(&out.stdout);
    assert_eq!(evs.last().unwrap()["state"], "succeeded", "eventos: {evs:?}");
    let files = Command::new("git").args(["ls-tree", "-r", "--name-only", "agent/issue-42"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    let files = String::from_utf8_lossy(&files.stdout);
    assert!(files.contains("fix.txt"), "o commit de correção não foi pushado: {files}");
    assert!(!files.contains("sem-commit.txt"), "sobra sem commit virou checkpoint: {files}");
}
