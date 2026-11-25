// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

//! `Infimum.Forma` — The fourth architectural layer of Infimum:
//! declarative spatial-interactive UI with Z3-verified geometry and zero-overhead
//! lowering into `Infimum.Flux` / `Infimum.Strata` 1-Draw-Call GPU primitive streams.

pub mod ast;
pub mod lower;
pub mod parser;
pub mod verify_ui;

pub use ast::{Align, FormaDef, FormaNode, FormaNodeKind, FormaProps, Justify, Overflow};
pub use lower::{forma_runtime_helpers, forma_runtime_helpers_with_text, lower_forma_def};
pub use verify_ui::{verify_forma, verify_module_formas, FormaVerifyReport, FORMA_MAX_PRIMS};
