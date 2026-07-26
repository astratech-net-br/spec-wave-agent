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
    #[serde(default = "d_impl_timeout")]
    pub implement_timeout_secs: u64,
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
fn d_impl_timeout() -> u64 { 3600 }
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
        assert_eq!(cfg.implement_timeout_secs, 3600);
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
    fn valida_intervalos_positivos() {
        for field in ["poll_interval_secs", "heartbeat_secs", "implement_timeout_secs"] {
            let cfg = parse(&format!("repo = \"a/b\"\n{field} = 0"));
            assert!(cfg.validate().is_err(), "deveria rejeitar {field}=0");
        }
        let cfg = parse("repo = \"a/b\"\nlease_ttl_secs = 0");
        assert!(cfg.validate().is_err());
    }
}
