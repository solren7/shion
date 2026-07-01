// Cross-cutting infra (LLM backend, Codex OAuth, tool adapter, workday calendar)
pub mod codex;
pub mod llm;
pub mod rendezvous;
pub mod rig_tool;
pub mod workday;

// Layered infra by concern
pub mod memory;
pub mod messaging;
pub mod persistence;
