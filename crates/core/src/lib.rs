// <<<LLMCTX
// FILE: crates/core/src/lib.rs
// ROLE: Root of the shared core library — re-exports all public modules
// EXPORTS: ads, config, extract, filter, fsutil, git, migrate, ollama, pack, process, runtime, store (modules)
// IMPORTS: NONE
// USED BY: crates/daemon/src/main.rs, crates/cli/src/main.rs, crates/cpctx/src/main.rs
// NOTES: NONE
// LLMCTX>>>

pub mod ads;
pub mod config;
pub mod extract;
pub mod filter;
pub mod fsutil;
pub mod git;
pub mod migrate;
pub mod ollama;
pub mod pack;
pub mod process;
pub mod runtime;
pub mod store;
