#![deny(clippy::todo, clippy::unimplemented, dead_code, unused_must_use)]

pub mod abi;
pub mod ast;
pub mod canon;
pub mod codegen;
pub mod diag;
pub mod eval;
pub mod forma;
pub mod lexer;
pub mod light_verify;
pub mod mcp;
pub mod parser;
pub mod showcase;
pub mod store;
pub mod strata_elf;
pub mod strata_wasm;
pub mod typecheck;
pub mod verify;
pub mod wasm;
pub mod wasm_api;
