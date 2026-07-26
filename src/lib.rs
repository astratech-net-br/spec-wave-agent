//! Biblioteca do spec-wave-agent — exposta para os testes de integração;
//! o binário (main.rs) é só orquestração sobre estes módulos.

pub mod config;
pub mod lease;
pub mod queue;
pub mod runner;
pub mod shell;
