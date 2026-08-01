//! Integração do executor de BUG (RFC-004 §7.3) contra um bare repo LOCAL e um
//! stub no lugar do Claude Code — sem rede, sem gh, sem npm.
//!
//! O que estes testes protegem: que o tipo do item escolhe comando, prompt e
//! timeout, e que o marcador estendido com a causa raiz sobrevive à
//! desserialização — inclusive quando o executor não preenche nada dele.

use spec_wave_agent::config::Config;
use spec_wave_agent::queue::{QueueItem, QueueKind};
use spec_wave_agent::runner::{ensure_workspace, run_item, RunEnd};
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

/// Origin bare + Config com stubs SEPARADOS para feature e bug: é assim que se
/// prova que o tipo do item escolhe o caminho, e não a configuração global.
fn setup(tmp: &TempDir, bug_stub: &str, bug_timeout: u64) -> Config {
    let origin = tmp.path().join("origin.git");
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

    let bug = tmp.path().join("bug-stub.sh");
    std::fs::write(&bug, format!("#!/usr/bin/env bash\nset -eu\n{bug_stub}\n")).unwrap();
    std::fs::set_permissions(&bug, std::fs::Permissions::from_mode(0o755)).unwrap();

    // O stub de FEATURE falha sempre: se o runner escolher o caminho errado,
    // o teste quebra em vez de passar por acidente.
    let feat = tmp.path().join("feature-stub.sh");
    std::fs::write(&feat, "#!/usr/bin/env bash\necho 'caminho de feature usado num BUG' >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&feat, std::fs::Permissions::from_mode(0o755)).unwrap();

    let toml_str = format!(r#"
        repo = "test/test"
        remote_url = "{origin}"
        workdir = "{workdir}"
        implement_timeout_secs = 60
        bug_timeout_secs = {bug_timeout}
        feature_command = "{feat} {{issue}}"
        feature_prompt = "NAO DEVE SER USADO"
        bug_command = "{bug} {{issue}}"
        bug_prompt = "corrija o bug {{issue}} em quatro fases: reproduzir, causa raiz, fix, regressao"
    "#, origin = origin.display(), workdir = tmp.path().join("wd").display(),
        feat = feat.display(), bug = bug.display());
    toml::from_str(&toml_str).unwrap()
}

fn channels() -> (watch::Sender<bool>, watch::Receiver<bool>,
                  watch::Sender<bool>, watch::Receiver<bool>) {
    let (lt, lr) = watch::channel(false);
    let (st, sr) = watch::channel(false);
    (lt, lr, st, sr)
}

fn item(number: u64) -> QueueItem {
    QueueItem { kind: QueueKind::Bug, number }
}

#[tokio::test]
async fn bug_usa_o_prompt_de_bug_e_devolve_a_causa_raiz() {
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, r#"
        cat > prompt-recebido.txt
        git add -A && git config user.name t && git config user.email t@t
        git commit -qm "fix: validacao aceitava vazio (#$1) [spec-wave-agent]"
        git push -q
        cat > .spec-wave-agent-result.json <<'JSON'
{"status":"ok","causa_raiz":"checkCpf aceita string vazia em validators.ts",
 "fix":"exige 11 digitos antes de validar",
 "teste_regressao":"validators.test.ts: cpf vazio deve falhar",
 "arquivos":["src/validators.ts","test/validators.test.ts"]}
JSON
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 42).await.unwrap();
    let end = run_item(&cfg, &ws, item(42), lr, sr).await.unwrap();

    let RunEnd::Success(result) = end else { panic!("esperava Success, veio {end:?}") };
    let r = result.expect("o marker deveria ter sido lido");
    assert_eq!(r.status, "ok");
    assert!(r.causa_raiz.unwrap().contains("checkCpf"));
    assert!(r.fix.unwrap().contains("11 digitos"));
    assert!(r.teste_regressao.unwrap().contains("cpf vazio"));
    assert_eq!(r.arquivos.unwrap().len(), 2);

    // O prompt entregue é o de BUG — não o de feature.
    let prompt = std::fs::read_to_string(ws.join("prompt-recebido.txt")).unwrap();
    assert!(prompt.contains("quatro fases"), "prompt recebido: {prompt}");
    assert!(prompt.contains("42"), "o placeholder {{issue}} não foi resolvido");
    assert!(!prompt.contains("NAO DEVE SER USADO"));
}

#[tokio::test]
async fn marker_sem_campos_de_rca_ainda_e_sucesso() {
    // Compatibilidade retroativa: um executor antigo (ou um bug_prompt
    // customizado pelo usuário) escreve só {"status":"ok"}. Os campos de RCA
    // são #[serde(default)] justamente para isso.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, r#"
        cat > /dev/null
        git add -A && git config user.name t && git config user.email t@t
        git commit -qm "fix: algo (#$1)" --allow-empty
        git push -q
        printf '{"status": "ok"}' > .spec-wave-agent-result.json
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 43).await.unwrap();
    let end = run_item(&cfg, &ws, item(43), lr, sr).await.unwrap();

    let RunEnd::Success(result) = end else { panic!("esperava Success, veio {end:?}") };
    let r = result.expect("marker mínimo ainda deve ser lido");
    assert_eq!(r.status, "ok");
    assert!(r.causa_raiz.is_none());
    assert!(r.arquivos.is_none());
}

#[tokio::test]
async fn bug_usa_o_timeout_de_bug_e_nao_o_de_feature() {
    // implement_timeout_secs = 60 no setup; bug_timeout_secs = 1 aqui. Um stub
    // que dorme 30s só pode ser morto se o teto aplicado for o do BUG.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, r#"
        cat > /dev/null
        sleep 30
    "#, 1);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 44).await.unwrap();
    let inicio = std::time::Instant::now();
    let end = run_item(&cfg, &ws, item(44), lr, sr).await.unwrap();

    assert!(matches!(end, RunEnd::Failed(_)), "esperava Failed por timeout, veio {end:?}");
    assert!(inicio.elapsed().as_secs() < 20,
            "o teto de feature (60s) foi aplicado a um bug: {:?}", inicio.elapsed());
}

#[tokio::test]
async fn marker_partial_de_bug_vira_falha_com_o_detalhe() {
    // "não consegui reproduzir" é o desfecho previsto pelo prompt — e precisa
    // chegar ao humano como falha, não como sucesso silencioso.
    let tmp = TempDir::new().unwrap();
    let cfg = setup(&tmp, r#"
        cat > /dev/null
        printf '{"status":"partial","detalhe":"nao consegui reproduzir com os passos do relato"}' \
          > .spec-wave-agent-result.json
    "#, 60);
    let (_lt, lr, _st, sr) = channels();

    let ws = ensure_workspace(&cfg, 45).await.unwrap();
    let end = run_item(&cfg, &ws, item(45), lr, sr).await.unwrap();

    let RunEnd::Failed(reason) = end else { panic!("esperava Failed, veio {end:?}") };
    assert!(reason.contains("nao consegui reproduzir"), "motivo: {reason}");
}
