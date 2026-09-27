//! Lease distribuído sobre git refs (CAS real).
//!
//! Invariantes — NÃO alterar sem revisar o protocolo inteiro:
//!   - Aquisição: push NÃO-forçado da ref (falha atômica se a ref existe;
//!     perder a corrida é resultado normal, não erro).
//!   - Renovação/roubo: `--force-with-lease=<ref>:<sha esperado>` (CAS no
//!     sha exato observado).
//!   - `generation` incrementa a cada aquisição/roubo (fencing token).
//!   - Perda do lease (RenewError::Lost) => o chamador deve fazer fencing
//!     imediato: matar o filho, sem checkpoint/release/push.

use crate::shell::{run, run_ok};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub issue: u64,
    pub owner: String,
    /// Fencing token: incrementa a cada aquisição/roubo
    pub generation: u64,
    pub heartbeat: DateTime<Utc>,
}

/// Um lease expirou se o último heartbeat é mais antigo que ttl_secs.
pub fn is_expired(heartbeat: DateTime<Utc>, now: DateTime<Utc>, ttl_secs: i64) -> bool {
    now - heartbeat > Duration::seconds(ttl_secs)
}

/// Resultado de falha do renew: distingue perda real do lease (fencing,
/// nunca retry) de erro transiente de rede (retry dentro do orçamento).
#[derive(Debug)]
pub enum RenewError {
    /// CAS rejeitado / lease tomado / ref sumiu => fencing imediato.
    Lost(String),
    /// Erro de rede/ambiente => retry permitido; o próximo retry re-executa
    /// o CAS e detecta Lost deterministicamente se o lease mudou de dono.
    Transient(anyhow::Error),
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenewError::Lost(m) => write!(f, "lease perdido: {m}"),
            RenewError::Transient(e) => write!(f, "erro transiente: {e:#}"),
        }
    }
}

/// Namespace padrão das refs de lease (o de sempre). `refs/heads/` é o que
/// todo host git aceita; o custo é aparecer na lista de branches e poder
/// disparar CI com `on: push` sem filtro (RFC-008 §6.9, P18).
pub const DEFAULT_LEASE_PREFIX: &str = "refs/heads/spec-wave-agent/claims";

#[derive(Clone)]
pub struct LeaseRepo {
    dir: PathBuf,
    prefix: String,
}

/// Prefixo aceito para refs de lease: `refs/...`, sem espaço, `..`, `~`, `^`,
/// `:`, `?`, `*`, `[` ou `\\` (regras de nome de ref do git) e sem barra no fim.
pub fn valid_lease_prefix(p: &str) -> bool {
    p.starts_with("refs/") && p.len() > 5 && !p.ends_with('/')
        && !p.contains("..") && !p.contains("//")
        && !p.chars().any(|c| c.is_whitespace() || "~^:?*[\\".contains(c))
}

impl LeaseRepo {
    pub async fn open(dir: PathBuf, remote_url: &str) -> Result<Self> {
        Self::open_with_prefix(dir, remote_url, DEFAULT_LEASE_PREFIX).await
    }

    /// `prefix`: namespace das refs de lease (ex.: `refs/spec-wave/claims`,
    /// fora de `refs/heads/` — não polui branches nem dispara CI).
    pub async fn open_with_prefix(dir: PathBuf, remote_url: &str, prefix: &str) -> Result<Self> {
        if !valid_lease_prefix(prefix) {
            bail!("prefixo de lease inválido: {prefix:?} (esperado refs/…)");
        }
        if !dir.join(".git").exists() {
            std::fs::create_dir_all(&dir)?;
            run_ok(&dir, "git", &["init", "--quiet"]).await?;
            run_ok(&dir, "git", &["remote", "add", "origin", remote_url]).await?;
        }
        // Identidade local: commit-tree exige user.name/email mesmo sem
        // worktree; não depender do gitconfig global da máquina.
        run_ok(&dir, "git", &["config", "user.name", "spec-wave-agent"]).await?;
        run_ok(&dir, "git", &["config", "user.email", "agent@spec-wave"]).await?;
        Ok(Self { dir, prefix: prefix.to_string() })
    }

    fn claim_ref(&self, issue: u64) -> String {
        format!("{}/{issue}", self.prefix)
    }

    /// Lê o lease atual do remoto. Retorna (sha_da_ref, lease) ou None.
    async fn fetch(&self, issue: u64) -> Result<Option<(String, Lease)>> {
        let r = self.claim_ref(issue);
        let ls = run(&self.dir, "git", &["ls-remote", "origin", &r]).await?;
        if !ls.ok {
            bail!("git ls-remote falhou: {}", ls.stderr.trim());
        }
        let sha = ls.stdout.split_whitespace().next().unwrap_or("").to_string();
        if sha.is_empty() {
            return Ok(None);
        }
        run_ok(&self.dir, "git",
               &["fetch", "--quiet", "origin", &format!("+{r}:{r}")]).await?;
        let content = run_ok(&self.dir, "git",
                             &["show", &format!("{sha}:lease.json")]).await?;
        let lease: Lease = serde_json::from_str(content.trim())
            .context("lease.json inválido")?;
        Ok(Some((sha, lease)))
    }

    /// Cria um commit contendo apenas lease.json (git plumbing, sem worktree).
    async fn make_commit(&self, lease: &Lease, parent: Option<&str>) -> Result<String> {
        let json = serde_json::to_string_pretty(lease)?;
        // blob
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        {
            use tokio::io::AsyncWriteExt;
            child.stdin.take().unwrap().write_all(json.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("git hash-object falhou");
        }
        let blob = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // tree
        let mut child = Command::new("git")
            .args(["mktree"])
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        {
            use tokio::io::AsyncWriteExt;
            let line = format!("100644 blob {blob}\tlease.json\n");
            child.stdin.take().unwrap().write_all(line.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("git mktree falhou");
        }
        let tree = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // commit
        let msg = format!("lease issue #{} gen {} by {}",
                          lease.issue, lease.generation, lease.owner);
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), msg];
        if let Some(p) = parent {
            args.push("-p".into());
            args.push(p.into());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let sha = run_ok(&self.dir, "git", &argv).await?;
        Ok(sha.trim().to_string())
    }

    /// Tenta adquirir o lease. Retorna Some(lease) se este agente é o dono.
    pub async fn try_acquire(&self, issue: u64, owner: &str, ttl: i64)
        -> Result<Option<Lease>>
    {
        let r = self.claim_ref(issue);
        match self.fetch(issue).await? {
            None => {
                // Ref não existe: push NÃO-forçado = CAS de criação.
                let lease = Lease {
                    issue, owner: owner.into(), generation: 1,
                    heartbeat: Utc::now(),
                };
                let sha = self.make_commit(&lease, None).await?;
                let push = run(&self.dir, "git",
                               &["push", "--quiet", "origin",
                                 &format!("{sha}:{r}")]).await?;
                Ok(push.ok.then_some(lease)) // perdeu a corrida => None
            }
            Some((cur_sha, cur)) => {
                if !is_expired(cur.heartbeat, Utc::now(), ttl) {
                    return Ok(None); // dono vivo
                }
                // STEAL: force-with-lease esperando exatamente o sha estagnado.
                let lease = Lease {
                    issue, owner: owner.into(),
                    generation: cur.generation + 1,
                    heartbeat: Utc::now(),
                };
                let sha = self.make_commit(&lease, Some(&cur_sha)).await?;
                let spec = format!("--force-with-lease={r}:{cur_sha}");
                let push = run(&self.dir, "git",
                               &["push", "--quiet", &spec, "origin",
                                 &format!("{sha}:{r}")]).await?;
                if push.ok {
                    tracing::warn!(target: "lease",
                        "roubado issue #{issue} gen {} (dono anterior: {}, \
                         parado desde {})",
                        lease.generation, cur.owner, cur.heartbeat);
                }
                Ok(push.ok.then_some(lease))
            }
        }
    }

    /// Renova o heartbeat via CAS. Lost => fencing imediato (abortar tudo);
    /// Transient => o chamador pode tentar de novo dentro do orçamento.
    pub async fn renew(&self, lease: &mut Lease) -> Result<(), RenewError> {
        let r = self.claim_ref(lease.issue);
        let (cur_sha, cur) = match self.fetch(lease.issue).await {
            Err(e) => return Err(RenewError::Transient(e)),
            Ok(None) => return Err(RenewError::Lost("ref do lease sumiu".into())),
            Ok(Some(v)) => v,
        };
        if cur.owner != lease.owner || cur.generation != lease.generation {
            return Err(RenewError::Lost(format!(
                "lease tomado por {} (gen {})", cur.owner, cur.generation)));
        }
        let renewed = Lease { heartbeat: Utc::now(), ..lease.clone() };
        let sha = self.make_commit(&renewed, Some(&cur_sha)).await
            .map_err(RenewError::Transient)?;
        let spec = format!("--force-with-lease={r}:{cur_sha}");
        let push = run(&self.dir, "git",
                       &["push", "--quiet", &spec, "origin",
                         &format!("{sha}:{r}")]).await
            .map_err(RenewError::Transient)?;
        if !push.ok {
            let s = format!("{}{}", push.stdout, push.stderr);
            // Rejeição do force-with-lease = CAS perdido; qualquer outra
            // falha (DNS, timeout) é transiente.
            if s.contains("stale info") || s.contains("[rejected]")
                || s.contains("[remote rejected]")
            {
                return Err(RenewError::Lost(
                    format!("renew rejeitado (CAS): {}", s.trim())));
            }
            return Err(RenewError::Transient(
                anyhow!("push do renew falhou: {}", s.trim())));
        }
        lease.heartbeat = renewed.heartbeat;
        Ok(())
    }

    pub async fn release(&self, issue: u64) -> Result<()> {
        let r = self.claim_ref(issue);
        let _ = run(&self.dir, "git",
                    &["push", "--quiet", "origin", &format!(":{r}")]).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixo_de_lease() {
        assert!(valid_lease_prefix(DEFAULT_LEASE_PREFIX));
        assert!(valid_lease_prefix("refs/spec-wave/claims"));
        for bad in ["refs/", "heads/x", "refs/a/", "refs/a..b", "refs/a b", "refs/a:b", "refs/a*", "refs//a"] {
            assert!(!valid_lease_prefix(bad), "{bad}");
        }
    }

    #[test]
    fn is_expired_fronteiras() {
        let now = Utc::now();
        // exatamente no ttl: NÃO expirado (comparação é estrita)
        assert!(!is_expired(now - Duration::seconds(600), now, 600));
        assert!(is_expired(now - Duration::seconds(601), now, 600));
        // heartbeat no futuro (clock skew): não expirado
        assert!(!is_expired(now + Duration::seconds(30), now, 600));
        // ttl zero: qualquer atraso positivo expira
        assert!(is_expired(now - Duration::seconds(1), now, 0));
        assert!(!is_expired(now, now, 0));
    }
}
