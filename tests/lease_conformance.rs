//! Suíte de conformidade do lease por provedor (RFC-008 §6.9, P18).
//!
//! Roda o protocolo do lease contra um REMOTO DE VERDADE — é o que diz se um
//! host git serve de árbitro: se o servidor rejeita o push não-forçado de uma
//! ref existente e o `--force-with-lease` com o sha errado, o lease é
//! exclusivo ali. Desligada por padrão (precisa de rede e de um repositório
//! com permissão de push):
//!
//!   SPEC_WAVE_LEASE_REMOTE=https://github.com/<org>/<repo>.git \
//!   SPEC_WAVE_LEASE_PREFIXES=refs/heads/spec-wave-agent/claims,refs/spec-wave/claims \
//!   cargo test --test lease_conformance -- --ignored --test-threads=1
//!
//! A credencial é a do git da máquina (credential helper / GIT_ASKPASS). Cada
//! caso usa um número de issue aleatório alto e libera o lease no fim.

use spec_wave_agent::lease::{LeaseRepo, RenewError, DEFAULT_LEASE_PREFIX};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn remote() -> Option<String> {
    std::env::var("SPEC_WAVE_LEASE_REMOTE").ok().filter(|s| !s.trim().is_empty())
}

fn prefixes() -> Vec<String> {
    std::env::var("SPEC_WAVE_LEASE_PREFIXES")
        .unwrap_or_else(|_| DEFAULT_LEASE_PREFIX.to_string())
        .split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn issue() -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64;
    900_000_000 + nanos % 99_000_000
}

async fn agents(tmp: &TempDir, url: &str, prefix: &str) -> (LeaseRepo, LeaseRepo) {
    let a = LeaseRepo::open_with_prefix(tmp.path().join("a"), url, prefix).await.unwrap();
    let b = LeaseRepo::open_with_prefix(tmp.path().join("b"), url, prefix).await.unwrap();
    (a, b)
}

fn ls_remote(url: &str, args: &[&str]) -> String {
    let mut all = vec!["ls-remote"];
    all.extend_from_slice(args);
    all.push(url);
    let out = Command::new("git").args(&all).output().expect("git ls-remote");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[tokio::test]
#[ignore]
async fn conformidade_do_lease() {
    let Some(url) = remote() else {
        eprintln!("SPEC_WAVE_LEASE_REMOTE ausente — suíte pulada");
        return;
    };
    for prefix in prefixes() {
        eprintln!("== {url} :: {prefix}");
        adquirir_e_dono_vivo_bloqueia(&url, &prefix).await;
        corrida_tem_um_vencedor(&url, &prefix).await;
        renovar_e_roubo_apos_ttl(&url, &prefix).await;
        liberar_devolve(&url, &prefix).await;
        onde_a_ref_aparece(&url, &prefix).await;
    }
}

async fn adquirir_e_dono_vivo_bloqueia(url: &str, prefix: &str) {
    let tmp = TempDir::new().unwrap();
    let (a, b) = agents(&tmp, url, prefix).await;
    let n = issue();
    let lease = a.try_acquire(n, "agente-a", 600).await.unwrap().expect("a adquire");
    assert_eq!(lease.generation, 1);
    assert!(b.try_acquire(n, "agente-b", 600).await.unwrap().is_none(), "dono vivo deveria bloquear b");
    a.release(n).await.unwrap();
    eprintln!("  ok  adquirir / dono vivo bloqueia");
}

async fn corrida_tem_um_vencedor(url: &str, prefix: &str) {
    let tmp = TempDir::new().unwrap();
    let (a, b) = agents(&tmp, url, prefix).await;
    let n = issue();
    let (ra, rb) = tokio::join!(a.try_acquire(n, "agente-a", 600), b.try_acquire(n, "agente-b", 600));
    let vencedores = [ra.unwrap().is_some(), rb.unwrap().is_some()].iter().filter(|x| **x).count();
    assert_eq!(vencedores, 1, "a criação da ref precisa ser CAS no servidor");
    a.release(n).await.unwrap();
    eprintln!("  ok  corrida: um vencedor");
}

async fn renovar_e_roubo_apos_ttl(url: &str, prefix: &str) {
    let tmp = TempDir::new().unwrap();
    let (a, b) = agents(&tmp, url, prefix).await;
    let n = issue();
    let mut la = a.try_acquire(n, "agente-a", 600).await.unwrap().expect("a adquire");
    a.renew(&mut la).await.expect("renew do dono");
    // TTL de 1 s: depois de 2 s sem heartbeat, b rouba com force-with-lease.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let lb = b.try_acquire(n, "agente-b", 1).await.unwrap().expect("b rouba após o TTL");
    assert_eq!(lb.generation, la.generation + 1);
    match a.renew(&mut la).await {
        Err(RenewError::Lost(_)) => {}
        other => panic!("o dono antigo precisa receber Lost (fencing), veio {other:?}"),
    }
    b.release(n).await.unwrap();
    eprintln!("  ok  renovar / roubo após TTL / fencing do dono antigo");
}

async fn liberar_devolve(url: &str, prefix: &str) {
    let tmp = TempDir::new().unwrap();
    let (a, b) = agents(&tmp, url, prefix).await;
    let n = issue();
    a.try_acquire(n, "agente-a", 600).await.unwrap().expect("a adquire");
    a.release(n).await.unwrap();
    let lb = b.try_acquire(n, "agente-b", 600).await.unwrap().expect("b adquire depois do release");
    assert_eq!(lb.generation, 1, "release apaga a ref: nova geração começa do 1");
    b.release(n).await.unwrap();
    eprintln!("  ok  release");
}

async fn onde_a_ref_aparece(url: &str, prefix: &str) {
    let tmp = TempDir::new().unwrap();
    let (a, _) = agents(&tmp, url, prefix).await;
    let n = issue();
    a.try_acquire(n, "agente-a", 600).await.unwrap().expect("a adquire");
    let reff = format!("{prefix}/{n}");
    let visivel = ls_remote(url, &[]).contains(&reff);
    let como_branch = ls_remote(url, &["--heads"]).contains(&reff);
    a.release(n).await.unwrap();
    assert!(visivel, "a ref do lease não aparece no ls-remote");
    assert_eq!(como_branch, prefix.starts_with("refs/heads/"));
    eprintln!("  ok  ref em {reff} (branch: {como_branch})");
}
