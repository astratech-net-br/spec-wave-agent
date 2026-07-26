//! Configuração do agente (~/.config/spec-wave-agent/config.toml).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// "owner/repo"
    pub repo: String,
    /// Label que marca itens na fila (aplicada por humano ou automação do board)
    #[serde(default = "d_queue_label")]
    pub queue_label: String,
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "d_heartbeat")]
    pub heartbeat_secs: u64,
    /// Sem heartbeat por este tempo => lease considerado morto (pode roubar)
    #[serde(default = "d_ttl")]
    pub lease_ttl_secs: i64,
    /// Tempo máximo de UMA FEATURE inteira (todas as stories)
    #[serde(default = "d_impl_timeout")]
    pub implement_timeout_secs: u64,
    /// Comando do executor da feature — argv separado por espaço (aspas
    /// simples/duplas suportadas; SEM shell). Placeholder: {issue}
    #[serde(default = "d_feature_command")]
    pub feature_command: String,
    /// Prompt de orquestração enviado ao executor via stdin. Placeholder: {issue}
    #[serde(default = "d_feature_prompt")]
    pub feature_prompt: String,
    /// Depois de processar uma issue (qualquer desfecho), ela fica fora da
    /// fila deste agente por este tempo — evita re-claim imediato por atraso
    /// do índice de busca do GitHub e loop de retry em falha.
    #[serde(default = "d_cooldown")]
    pub cooldown_secs: u64,
    /// Máximo de rodadas do executor por feature: exit 0 sem marker relança
    /// o executor para continuar (ele retoma pelo estado do git/spec-wave).
    #[serde(default = "d_max_rounds")]
    pub max_executor_rounds: u32,
    /// Diretório de trabalho do agente
    #[serde(default = "d_workdir")]
    pub workdir: String,
    /// Identidade do agente (default: usuario@hostname)
    pub agent_id: Option<String>,
    /// Override da URL do remoto (testes / git self-hosted). Default: GitHub.
    pub remote_url: Option<String>,
}

fn d_queue_label() -> String { "spec-wave:dev-agent".into() }
fn d_poll() -> u64 { 60 }
fn d_heartbeat() -> u64 { 120 }
fn d_ttl() -> i64 { 600 }
fn d_impl_timeout() -> u64 { 14400 }
fn d_feature_command() -> String {
    // stream-json: cada evento (texto, tool calls, resultado) vira uma linha
    // JSON no stdout, que o runner formata para o console (ver runner.rs).
    "claude -p --output-format stream-json --verbose \
     --permission-mode acceptEdits \
     --allowedTools \"Bash(npx:*),Bash(git:*),Edit,Write,Read,Glob,Grep,Task\""
        .into()
}
fn d_cooldown() -> u64 { 900 }
fn d_max_rounds() -> u32 { 8 }
fn d_feature_prompt() -> String {
    "Você está no clone do repositório, no branch de trabalho da Feature #{issue}.\n\
     Implemente a feature completa usando o spec-wave:\n\
     1. Rode `npx spec-wave order {issue}` para obter as user stories e a \
     ordem de dependência.\n\
     2. Implemente cada story rodando `npx spec-wave implement <número>`, \
     respeitando a ordem: uma story só pode começar depois que TODAS as suas \
     dependências estiverem concluídas.\n\
     3. Stories independentes entre si podem ser implementadas em paralelo \
     com sub-agentes.\n\
     4. Ao concluir cada story: rode os testes relevantes, commite neste \
     branch com mensagem \"feat: story #<número> [spec-wave-agent]\" e faça push.\n\
     5. Se uma story falhar, pule as que dependem dela e continue as \
     independentes; ao final, relate o que falhou.\n\
     \n\
     Regras OBRIGATÓRIAS:\n\
     - Você roda em RODADAS: se o seu turno terminar sem o marker de \
     conclusão (abaixo), você será REINVOCADO para continuar. Ao iniciar, \
     verifique o que já foi feito (`git log --oneline -20` e \
     `npx spec-wave order {issue}`) e continue de onde parou — não refaça \
     stories já commitadas.\n\
     - Processos em background que você deixar ao encerrar o turno são \
     MORTOS. Prefira executar cada implement em foreground e aguardar; se \
     usar background, aguarde a conclusão AINDA NESTE turno antes de \
     encerrar.\n\
     - Commite e pushe cada story assim que concluída (mensagem \
     \"feat: story #<número> [spec-wave-agent]\").\n\
     - SOMENTE quando TODAS as stories estiverem commitadas e pushadas (ou \
     declaradas como falha), escreva o arquivo \
     ./.spec-wave-agent-result.json (NÃO commite este arquivo) com \
     exatamente:\n\
     {\"status\": \"ok\"} se todas as stories foram implementadas, \
     commitadas e pushadas; ou\n\
     {\"status\": \"partial\", \"detalhe\": \"<o que falhou>\"} se alguma \
     story falhou definitivamente.\n\
     - NÃO escreva o marker se ainda houver stories pendentes que você \
     pretende continuar na próxima rodada.\n"
        .into()
}

/// Split de linha de comando em argv: espaços separam, aspas simples/duplas
/// agrupam (sem escapes; SEM shell — sem pipes/expansões).
pub fn split_command(s: &str) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => { quote = Some(c); has_token = true; }
            None if c.is_whitespace() => {
                if has_token { argv.push(std::mem::take(&mut cur)); has_token = false; }
            }
            None => { cur.push(c); has_token = true; }
        }
    }
    if quote.is_some() {
        bail!("aspas não fechadas em: {s:?}");
    }
    if has_token { argv.push(cur); }
    if argv.is_empty() {
        bail!("comando vazio");
    }
    Ok(argv)
}

/// Substitui o placeholder {issue}.
pub fn render_template(t: &str, issue: u64) -> String {
    t.replace("{issue}", &issue.to_string())
}
fn d_workdir() -> String {
    dirs_home().join(".spec-wave-agent").to_string_lossy().into_owned()
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn load_config() -> Result<Config> {
    let path = dirs_home().join(".config/spec-wave-agent/config.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("config não encontrada em {}", path.display()))?;
    Ok(toml::from_str(&raw)?)
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let parts: Vec<&str> = self.repo.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|p| p.is_empty() || p.contains(char::is_whitespace))
        {
            bail!("config: repo deve ter o formato \"owner/repo\" (recebido: {:?})",
                  self.repo);
        }
        for (name, v) in [
            ("poll_interval_secs", self.poll_interval_secs),
            ("heartbeat_secs", self.heartbeat_secs),
            ("implement_timeout_secs", self.implement_timeout_secs),
        ] {
            if v == 0 {
                bail!("config: {name} deve ser > 0");
            }
        }
        if self.lease_ttl_secs <= 0 {
            bail!("config: lease_ttl_secs deve ser > 0");
        }
        if (self.lease_ttl_secs as u64) < 4 * self.heartbeat_secs {
            bail!("config: lease_ttl_secs ({}) deve ser >= 4x heartbeat_secs ({}) \
                   para tolerar lentidão de rede sem roubo indevido",
                  self.lease_ttl_secs, self.heartbeat_secs);
        }
        if self.workdir.trim().is_empty() {
            bail!("config: workdir vazio");
        }
        split_command(&self.feature_command)
            .map_err(|e| anyhow::anyhow!("config: feature_command inválido: {e}"))?;
        if self.feature_prompt.trim().is_empty() {
            bail!("config: feature_prompt vazio");
        }
        if self.max_executor_rounds == 0 {
            bail!("config: max_executor_rounds deve ser >= 1");
        }
        Ok(())
    }

    pub fn remote_url(&self) -> String {
        self.remote_url.clone()
            .unwrap_or_else(|| format!("https://github.com/{}.git", self.repo))
    }

    pub fn agent_id(&self) -> String {
        self.agent_id.clone().unwrap_or_else(|| {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "agent".into());
            format!("{user}@{}", hostname())
        })
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn defaults_com_apenas_repo() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.queue_label, "spec-wave:dev-agent");
        assert_eq!(cfg.poll_interval_secs, 60);
        assert_eq!(cfg.heartbeat_secs, 120);
        assert_eq!(cfg.lease_ttl_secs, 600);
        assert_eq!(cfg.implement_timeout_secs, 14400);
        assert!(cfg.agent_id.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn remote_url_default_e_override() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.remote_url(), "https://github.com/org/repo.git");
        let cfg = parse("repo = \"org/repo\"\nremote_url = \"/tmp/origin.git\"");
        assert_eq!(cfg.remote_url(), "/tmp/origin.git");
    }

    #[test]
    fn valida_formato_do_repo() {
        for bad in ["foo", "a/b/c", "/b", "a/", "a b/c"] {
            let cfg = parse(&format!("repo = \"{bad}\""));
            assert!(cfg.validate().is_err(), "deveria rejeitar {bad:?}");
        }
    }

    #[test]
    fn valida_ttl_vs_heartbeat() {
        let cfg = parse("repo = \"a/b\"\nheartbeat_secs = 120\nlease_ttl_secs = 479");
        assert!(cfg.validate().is_err());
        let cfg = parse("repo = \"a/b\"\nheartbeat_secs = 120\nlease_ttl_secs = 480");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn defaults_do_executor_de_feature() {
        let cfg = parse(r#"repo = "org/repo""#);
        assert_eq!(cfg.implement_timeout_secs, 14400);
        let argv = split_command(&cfg.feature_command).unwrap();
        assert_eq!(argv[0], "claude");
        assert!(argv.contains(&"acceptEdits".to_string()));
        // allowedTools com vírgulas fica em UM token (estava entre aspas)
        assert!(argv.iter().any(|a| a.starts_with("Bash(npx:*),")));
        assert!(cfg.feature_prompt.contains("{issue}"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn split_command_com_aspas() {
        assert_eq!(split_command("a b c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(split_command("claude --allowedTools \"Bash(npx spec-wave:*)\"").unwrap(),
                   vec!["claude", "--allowedTools", "Bash(npx spec-wave:*)"]);
        assert_eq!(split_command("x 'com espaço' z").unwrap(),
                   vec!["x", "com espaço", "z"]);
        assert_eq!(split_command("x ''").unwrap(), vec!["x", ""]);
        assert!(split_command("  ").is_err());
        assert!(split_command("a 'aberto").is_err());
    }

    #[test]
    fn render_do_placeholder() {
        assert_eq!(render_template("spec-wave order {issue} #{issue}", 7),
                   "spec-wave order 7 #7");
    }

    #[test]
    fn valida_executor() {
        let cfg = parse("repo = \"a/b\"\nfeature_command = \"\"");
        assert!(cfg.validate().is_err());
        let cfg = parse("repo = \"a/b\"\nfeature_prompt = \" \"");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valida_intervalos_positivos() {
        for field in ["poll_interval_secs", "heartbeat_secs", "implement_timeout_secs"] {
            let cfg = parse(&format!("repo = \"a/b\"\n{field} = 0"));
            assert!(cfg.validate().is_err(), "deveria rejeitar {field}=0");
        }
        let cfg = parse("repo = \"a/b\"\nlease_ttl_secs = 0");
        assert!(cfg.validate().is_err());
    }
}
