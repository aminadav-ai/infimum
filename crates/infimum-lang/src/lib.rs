#![deny(clippy::todo, clippy::unimplemented, dead_code, unused_must_use)]

pub mod abi;
pub mod ast;
pub mod canon;
pub mod diag;
pub mod eval;
pub mod forma;
pub mod lexer;
pub mod mcp;
pub mod parser;
pub mod passport_crypto;
pub mod store;
pub mod strata_wasm;
pub mod typecheck;
pub mod verify;
pub mod wasm_api;
