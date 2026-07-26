//! Testes de integração do lease contra um bare repo LOCAL como origin —
//! sem rede, sem GitHub. Dois LeaseRepo simulam dois agentes.

use spec_wave_agent::lease::{LeaseRepo, RenewError};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Cria origin bare + abre dois LeaseRepo ("agente-a" e "agente-b").
async fn setup(tmp: &TempDir) -> (LeaseRepo, LeaseRepo) {
    let origin = tmp.path().join("origin.git");
    let st = Command::new("git")
        .args(["init", "--quiet", "--bare", origin.to_str().unwrap()])
        .status()
        .expect("git init --bare");
    assert!(st.success());
    let url = origin.to_str().unwrap();
    let a = LeaseRepo::open(tmp.path().join("agent-a"), url).await.unwrap();
    let b = LeaseRepo::open(tmp.path().join("agent-b"), url).await.unwrap();
    (a, b)
}

fn ref_exists(origin: &Path, issue: u64) -> bool {
    let out = Command::new("git")
        .args(["ls-remote", origin.to_str().unwrap(),
               &format!("refs/heads/spec-wave-agent/claims/{issue}")])
        .output()
        .expect("git ls-remote");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[tokio::test]
async fn acquire_basico_e_dono_vivo_bloqueia() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = setup(&tmp).await;

    let lease = a.try_acquire(7, "agente-a", 600).await.unwrap()
        .expect("agente-a deveria adquirir");
    assert_eq!(lease.generation, 1);
    assert_eq!(lease.owner, "agente-a");

    // Dono vivo (heartbeat recente): B não consegue.
    assert!(b.try_acquire(7, "agente-b", 600).await.unwrap().is_none());
}

#[tokio::test]
async fn corrida_concorrente_exatamente_um_vence() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = setup(&tmp).await;

    let (ra, rb) = tokio::join!(
        a.try_acquire(1, "agente-a", 600),
        b.try_acquire(1, "agente-b", 600),
    );
    let winners = [ra.unwrap(), rb.unwrap()];
    let n = winners.iter().filter(|w| w.is_some()).count();
    assert_eq!(n, 1, "exatamente um agente deve vencer a corrida");
}

#[tokio::test]
async fn renew_avanca_heartbeat_sem_mudar_generation() {
    let tmp = TempDir::new().unwrap();
    let (a, _b) = setup(&tmp).await;

    let mut lease = a.try_acquire(2, "agente-a", 600).await.unwrap().unwrap();
    let hb0 = lease.heartbeat;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    a.renew(&mut lease).await.expect("renew do dono deve funcionar");
    assert!(lease.heartbeat > hb0);
    assert_eq!(lease.generation, 1);
}

#[tokio::test]
async fn roubo_apos_expirar_e_fencing_do_antigo_dono() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = setup(&tmp).await;

    let mut lease_a = a.try_acquire(3, "agente-a", 0).await.unwrap().unwrap();

    // ttl = 0: qualquer atraso positivo expira => B pode roubar.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let lease_b = b.try_acquire(3, "agente-b", 0).await.unwrap()
        .expect("agente-b deveria roubar o lease expirado");
    assert_eq!(lease_b.generation, 2, "roubo incrementa o fencing token");
    assert_eq!(lease_b.owner, "agente-b");

    // Fencing: o renew do antigo dono deve falhar com Lost (nunca Transient).
    let err = a.renew(&mut lease_a).await
        .expect_err("renew do dono roubado deve falhar");
    assert!(matches!(err, RenewError::Lost(_)),
            "esperava Lost, veio: {err}");
}

#[tokio::test]
async fn release_deleta_ref_e_generation_reinicia() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = setup(&tmp).await;
    let origin = tmp.path().join("origin.git");

    let lease = a.try_acquire(4, "agente-a", 600).await.unwrap().unwrap();
    assert_eq!(lease.generation, 1);
    assert!(ref_exists(&origin, 4));

    a.release(4).await.unwrap();
    assert!(!ref_exists(&origin, 4), "release deve deletar a ref");

    // Semântica atual (pinada de propósito): ref deletada => novo ciclo
    // de vida do lease, generation volta a 1.
    let lease = b.try_acquire(4, "agente-b", 600).await.unwrap().unwrap();
    assert_eq!(lease.generation, 1);
}
