// <<<LLMCTX
// FILE: crates/core/src/lib.rs
// ROLE: Root of the shared core library — re-exports all public modules
// EXPORTS: ads, ollama, config, extract, process (modules)
// IMPORTS: NONE
// USED BY: crates/daemon/src/main.rs, crates/cli/src/main.rs
// NOTES: NONE
// LLMCTX>>>

pub mod ads;
pub mod config;
pub mod extract;
pub mod ollama;
pub mod process;
