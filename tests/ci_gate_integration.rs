//! Gate de CI (`run_item_gated`) contra um bare repo LOCAL, com stubs no lugar
//! do executor, do `spec-wave pr open` e do comando de checks — sem rede.
//!
//! O stub de checks decide pelo conteúdo do clone: `ci-red.txt` presente no
//! HEAD = CI vermelho. É o que um CI de verdade faz — julga o commit.

use spec_wave_agent::config::Config;
use spec_wave_agent::queue::{QueueItem, QueueKind};
use spec_wave_agent::runner::{ensure_workspace, run_item_gated, Gated, RunEnd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use tokio::sync::watch;

fn sh(cwd: &Path, script: &str) {
    let st = Command::new("bash").args(["-ceu", script]).current_dir(cwd).status().expect("bash");
    assert!(st.success(), "script falhou: {script}");
}

fn exe(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Executor padrão: na 1ª rodada entrega trabalho que o CI reprova; quando o
/// prompt traz a seção de correção, executa `on_fix`. Guarda cada prompt.
fn executor(on_fix: &str) -> String {
    format!(r#"
        n=$(ls ../prompts 2>/dev/null | wc -l)
        mkdir -p ../prompts
        cat > ../prompts/rodada-$n.txt
        git config user.name t; git config user.email t@t
        if grep -q "CORREÇÃO DE CI" ../prompts/rodada-$n.txt; then
            {on_fix}
        else
            echo quebrado > ci-red.txt
            git add -A && git commit -qm "feat: story" && git push -q
        fi
        printf '{{"status": "ok"}}' > .spec-wave-agent-result.json
    "#)
}

const CHECKS_BY_COMMIT: &str = r#"
    echo "$@" >> "$(dirname "$0")/checks-chamadas.txt"
    head=$(git rev-parse HEAD)
    if [ -f ci-red.txt ]; then
        printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"ci","workflowName":"ci","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://ci.example/job/1"}]}' "$head"
    else
        printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"ci","workflowName":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}' "$head"
    fi
"#;

fn setup(tmp: &TempDir, executor_body: &str, checks_body: &str, extra_toml: &str) -> Config {
    let origin = tmp.path().join("origin.git");
    sh(tmp.path(), &format!(
        "git init -q --bare -b main {o}
         git init -q -b main seed && cd seed
         git config user.name t && git config user.email t@t
         echo base > README.md && git add -A && git commit -qm base
         git remote add origin {o} && git push -q origin main",
        o = origin.display()));
    exe(&tmp.path().join("executor.sh"), executor_body);
    exe(&tmp.path().join("checks.sh"), checks_body);
    exe(&tmp.path().join("pr.sh"), r#"echo '{"url":"https://scm.example/t/t/pull/1","number":"1","created":true}'"#);
    toml::from_str(&format!(r#"
        repo = "test/test"
        remote_url = "{origin}"
        cli_command = "npx spec-wave"
        workdir = "{workdir}"
        implement_timeout_secs = 60
        feature_command = "{exec}"
        feature_prompt = "orquestre a feature {{issue}}"
        checks_command = "{checks}"
        ci_poll_secs = 1
        ci_timeout_secs = 20
        ci_no_checks_grace_secs = 2
        {extra_toml}
    "#, origin = origin.display(), workdir = tmp.path().join("wd").display(),
        exec = tmp.path().join("executor.sh").display(),
        checks = tmp.path().join("checks.sh").display())).unwrap()
}

async fn run(tmp: &TempDir, cfg: &Config) -> (RunEnd, PathBuf) {
    let (gated, hub) = run_gated(tmp, cfg, QueueKind::Feature).await;
    (gated.end, hub)
}

async fn run_gated(tmp: &TempDir, cfg: &Config, kind: QueueKind) -> (Gated, PathBuf) {
    let (_lt, lr) = watch::channel(false);
    let (_st, sr) = watch::channel(false);
    let ws = ensure_workspace(cfg, 7).await.unwrap();
    let pr = vec![tmp.path().join("pr.sh").to_string_lossy().into_owned()];
    let gated = run_item_gated(cfg, &ws, QueueItem { kind, number: 7 },
                               lr, sr, None, &pr).await.unwrap();
    (gated, ws.hub)
}

fn prompts(hub: &Path) -> Vec<String> {
    let dir = hub.parent().unwrap().join("prompts");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    files.iter().map(|f| std::fs::read_to_string(f).unwrap()).collect()
}

fn origin_log(tmp: &TempDir) -> String {
    let out = Command::new("git").args(["log", "--format=%s", "agent/issue-7"])
        .current_dir(tmp.path().join("origin.git")).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn ci_vermelho_relanca_o_executor_com_a_falha_e_sai_verde() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor(
        r#"git rm -q ci-red.txt && git commit -qm "fix(ci): corrige o teste" && git push -q"#),
        CHECKS_BY_COMMIT, "");
    let (end, hub) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "esperava Success, veio {end:?}");

    let prompts = prompts(&hub);
    assert_eq!(prompts.len(), 2, "uma rodada de trabalho + uma de correção");
    assert!(!prompts[0].contains("CORREÇÃO DE CI"));
    // A rodada de correção recebe o prompt original + PR, check e link.
    for needle in ["orquestre a feature 7", "https://scm.example/t/t/pull/1",
                   "Check reprovado: ci", "https://ci.example/job/1"] {
        assert!(prompts[1].contains(needle), "faltou {needle:?} em: {}", prompts[1]);
    }
    assert!(origin_log(&tmp).contains("fix(ci): corrige o teste"));
    // O comando de checks recebe a URL do PR e o contrato de campos.
    let calls = std::fs::read_to_string(tmp.path().join("checks-chamadas.txt")).unwrap();
    assert!(calls.contains("https://scm.example/t/t/pull/1 --json headRefOid,statusCheckRollup"), "{calls}");
}

#[tokio::test]
async fn ci_que_segue_vermelho_falha_depois_do_teto_de_correcoes() {
    let tmp = TempDir::new().unwrap();
    // A "correção" commita, mas não conserta nada.
    let cfg = setup(&tmp, &executor(
        r#"date +%N >> tentativa.txt && git add -A && git commit -qm "fix(ci): tentativa" && git push -q"#),
        CHECKS_BY_COMMIT, "max_ci_fix_rounds = 2");
    let (end, hub) = run(&tmp, &cfg).await;
    let RunEnd::Failed(reason) = end else { panic!("esperava Failed, veio {end:?}") };
    assert!(reason.contains("após 2 rodada(s) de correção"), "{reason}");
    assert!(reason.contains("https://scm.example/t/t/pull/1: ci"), "{reason}");
    assert_eq!(prompts(&hub).len(), 3, "trabalho + 2 correções");
}

#[tokio::test]
async fn correcao_sem_commit_falha_em_vez_de_esperar_o_mesmo_ci() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"), CHECKS_BY_COMMIT, "");
    let (end, hub) = run(&tmp, &cfg).await;
    let RunEnd::Failed(reason) = end else { panic!("esperava Failed, veio {end:?}") };
    assert!(reason.contains("sem commit novo"), "{reason}");
    assert_eq!(prompts(&hub).len(), 2);
}

#[tokio::test]
async fn sem_checks_ou_sem_como_consultar_nao_reprova() {
    // Repositório sem CI: rollup vazio até o fim da tolerância.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"),
        r#"printf '{"headRefOid":"%s","statusCheckRollup":[]}' "$(git rev-parse HEAD)""#, "");
    let (end, hub) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "sem checks: {end:?}");
    assert_eq!(prompts(&hub).len(), 1);

    // Comando de checks que falha sempre (sem credencial, SCM sem suporte):
    // gate pulado — mas só depois de 3 tentativas.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"),
        r#"echo x >> "$(dirname "$0")/tentativas"; echo 'sem credencial' >&2; exit 1"#, "");
    let (end, _) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "checks indisponíveis: {end:?}");
    assert_eq!(std::fs::read_to_string(tmp.path().join("tentativas")).unwrap().lines().count(), 3);
}

#[tokio::test]
async fn falha_transitoria_na_primeira_consulta_nao_desliga_o_gate() {
    // Um 502 logo depois de abrir o PR: a consulta seguinte vê o CI vermelho.
    let tmp = TempDir::new().unwrap();
    let checks = format!(r#"
        if [ ! -f "$(dirname "$0")/ja-falhou" ]; then
            touch "$(dirname "$0")/ja-falhou"; echo 'HTTP 502' >&2; exit 1
        fi
        {CHECKS_BY_COMMIT}"#);
    let cfg = setup(&tmp, &executor(
        r#"git rm -q ci-red.txt && git commit -qm "fix(ci): corrige" && git push -q"#), &checks, "");
    let (end, hub) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "{end:?}");
    assert_eq!(prompts(&hub).len(), 2, "o vermelho foi visto e corrigido");
}

#[tokio::test]
async fn vermelho_espera_os_checks_em_andamento_antes_de_agir() {
    // Um job reprovou e outro ainda roda: o log só existe quando o run
    // termina, então o gate não relança o executor antes disso.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor(
        r#"git rm -q ci-red.txt && git commit -qm "fix(ci): corrige" && git push -q"#), r#"
        head=$(git rev-parse HEAD)
        if [ ! -f ci-red.txt ]; then
            printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}' "$head"; exit 0
        fi
        n=$(cat "$(dirname "$0")/n" 2>/dev/null || echo 0); echo $((n+1)) > "$(dirname "$0")/n"
        lint=IN_PROGRESS; [ "$n" -ge 2 ] && lint=COMPLETED
        printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"},{"name":"lint","status":"%s","conclusion":"SUCCESS"}]}' "$head" "$lint"
    "#, "");
    let (end, hub) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "{end:?}");
    let n: u32 = std::fs::read_to_string(tmp.path().join("n")).unwrap().trim().parse().unwrap();
    assert_eq!(n, 3, "esperou o lint terminar antes de relançar o executor");
    assert!(prompts(&hub)[1].contains("Check reprovado: test"));
}

#[tokio::test]
async fn desfecho_leva_os_prs_e_o_resultado_do_primeiro_ok() {
    // Bug: a causa raiz vem do PRIMEIRO marker; o da rodada de correção de CI
    // não pode apagá-la.
    let tmp = TempDir::new().unwrap();
    let mut cfg = setup(&tmp, r#"
        n=$(ls ../prompts 2>/dev/null | wc -l); mkdir -p ../prompts
        cat > ../prompts/rodada-$n.txt
        git config user.name t; git config user.email t@t
        if grep -q "CORREÇÃO DE CI" ../prompts/rodada-$n.txt; then
            git rm -q ci-red.txt && git commit -qm "fix(ci): corrige" && git push -q
            printf '{"status": "ok"}' > .spec-wave-agent-result.json
        else
            echo quebrado > ci-red.txt
            git add -A && git commit -qm "fix: bug" && git push -q
            printf '{"status": "ok", "causa_raiz": "off-by-one em parse()"}' > .spec-wave-agent-result.json
        fi
    "#, CHECKS_BY_COMMIT, "");
    cfg.bug_command = cfg.feature_command.clone();
    let (gated, _) = run_gated(&tmp, &cfg, QueueKind::Bug).await;
    assert_eq!(gated.prs, vec!["https://scm.example/t/t/pull/1".to_string()]);
    let RunEnd::Success(Some(result)) = gated.end else { panic!("esperava Success com marker") };
    assert_eq!(result.causa_raiz.as_deref(), Some("off-by-one em parse()"));

    // Falha por CI vermelho também devolve o PR que ficou aberto.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"), CHECKS_BY_COMMIT, "max_ci_fix_rounds = 0");
    let (gated, _) = run_gated(&tmp, &cfg, QueueKind::Feature).await;
    assert!(matches!(gated.end, RunEnd::Failed(_)), "{:?}", gated.end);
    assert_eq!(gated.prs.len(), 1);
}

#[tokio::test]
async fn checks_do_commit_antigo_nao_valem_e_pendente_espera() {
    // O PR demora a refletir o push (headRefOid antigo, com CI vermelho do
    // commit anterior) e depois o check fica em andamento antes de passar.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"), r#"
        n=$(cat "$(dirname "$0")/n" 2>/dev/null || echo 0); echo $((n+1)) > "$(dirname "$0")/n"
        head=$(git rev-parse HEAD)
        case $n in
          0) printf '{"headRefOid":"antigo","statusCheckRollup":[{"name":"ci","status":"COMPLETED","conclusion":"FAILURE"}]}';;
          1) printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"ci","status":"IN_PROGRESS"}]}' "$head";;
          *) printf '{"headRefOid":"%s","statusCheckRollup":[{"name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}' "$head";;
        esac
    "#, "");
    let (end, hub) = run(&tmp, &cfg).await;
    assert!(matches!(end, RunEnd::Success(_)), "esperava Success, veio {end:?}");
    assert_eq!(prompts(&hub).len(), 1, "nenhuma rodada de correção");
    let n: u32 = std::fs::read_to_string(tmp.path().join("n")).unwrap().trim().parse().unwrap();
    assert!(n >= 4, "verde precisa de duas consultas seguidas (consultas: {n})");
}

#[tokio::test]
async fn gate_desligado_aceita_o_marker_sem_consultar_nada() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, &executor("true"), CHECKS_BY_COMMIT, "ci_gate = false");
    let (gated, hub) = run_gated(&tmp, &cfg, QueueKind::Feature).await;
    // O PR continua sendo aberto — só a espera pelos checks é que some.
    assert_eq!(gated.prs.len(), 1);
    let end = gated.end;
    assert!(matches!(end, RunEnd::Success(_)), "{end:?}");
    assert_eq!(prompts(&hub).len(), 1);
    assert!(!tmp.path().join("checks-chamadas.txt").exists());
}
