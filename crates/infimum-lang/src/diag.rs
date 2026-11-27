use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn zero() -> Self {
        Self {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticData {
    pub code: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    pub message: String,
    pub span: Span,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    pub repair_hint: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counterexample: BTreeMap<String, String>,
}

/// Unified JSON envelope for `infimum verify --json` and `infimum compile-wasm --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonDiagnosticEnvelope {
    pub ok: bool,
    pub phase: String,
    pub diagnostics: Vec<InfimumDiagnostic>,
}

/// Extract a 1-based source line from file contents for human/LLM diagnostics.
pub fn extract_source_line(src: &str, line_1based: usize) -> Option<String> {
    if line_1based == 0 {
        return None;
    }
    src.lines().nth(line_1based - 1).map(|l| l.to_string())
}

/// Pointer-sized (8-byte) machine-readable diagnostic for fast Result passing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InfimumDiagnostic(Box<DiagnosticData>);

impl Deref for InfimumDiagnostic {
    type Target = DiagnosticData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl InfimumDiagnostic {
    pub fn new(
        code: impl Into<String>,
        phase: impl Into<String>,
        message: impl Into<String>,
        span: Span,
        repair_hint: impl Into<String>,
    ) -> Self {
        Self(Box::new(DiagnosticData {
            code: code.into(),
            phase: phase.into(),
            file: None,
            function: None,
            message: message.into(),
            span,
            source_line: None,
            repair_hint: repair_hint.into(),
            counterexample: BTreeMap::new(),
        }))
    }

    pub fn with_counterexample(mut self, cex: BTreeMap<String, String>) -> Self {
        self.0.counterexample = cex;
        self
    }

    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        self.0.file = Some(file.into());
        self
    }

    pub fn with_function(mut self, func: impl Into<String>) -> Self {
        self.0.function = Some(func.into());
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.0.span = span;
        self
    }

    pub fn with_source_line(mut self, line: impl Into<String>) -> Self {
        self.0.source_line = Some(line.into());
        self
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| {
            format!(
                "{{\"code\":\"{}\",\"message\":\"{}\"}}",
                self.code, self.message
            )
        })
    }
}

impl fmt::Display for InfimumDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] ({}:{}): {}",
            self.code, self.span.line, self.span.col, self.message
        )?;
        if !self.counterexample.is_empty() {
            write!(f, "\n  counterexample: {{ ")?;
            for (i, (k, v)) in self.counterexample.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{k} = {v}")?;
            }
            write!(f, " }}")?;
        }
        write!(f, "\n  hint: {}", self.repair_hint)
    }
}

impl std::error::Error for InfimumDiagnostic {}
