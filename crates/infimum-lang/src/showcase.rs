use crate::diag::{InfimumDiagnostic, Span};
use crate::parser::parse_module;
use crate::store::{GraphStore, IngestEntry};
use crate::wasm::build_wasm_module;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowcaseDiagnosticsBundle {
    pub wasm_size_bytes: u64,
    pub ingested_nodes: Vec<IngestEntry>,
    pub payment_source: String,
    pub anti_stub_source: String,
    pub anti_stub_diagnostic: InfimumDiagnostic,
    pub secret_leak_source: String,
    pub secret_leak_diagnostic: InfimumDiagnostic,
}

pub fn build_showcase_assets(
    repo_root: &Path,
    showcase_dir: &Path,
) -> Result<ShowcaseDiagnosticsBundle, InfimumDiagnostic> {
    fs::create_dir_all(showcase_dir).map_err(|e| {
        InfimumDiagnostic::new(
            "E090_SHOWCASE_IO",
            "showcase",
            format!("Failed to create showcase directory: {e}"),
            Span::zero(),
            "Check write permissions on showcase output directory",
        )
    })?;

    let payment_src = fs::read_to_string(repo_root.join("examples/payment_demo.ifm")).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "showcase",
            format!("Cannot read examples/payment_demo.ifm: {e}"),
            Span::zero(),
            "Ensure examples/payment_demo.ifm exists",
        )
    })?;
    let anti_stub_src =
        fs::read_to_string(repo_root.join("examples/anti_stub_demo.ifm")).map_err(|e| {
            InfimumDiagnostic::new(
                "E040_FILE_READ",
                "showcase",
                format!("Cannot read examples/anti_stub_demo.ifm: {e}"),
                Span::zero(),
                "Ensure examples/anti_stub_demo.ifm exists",
            )
        })?;
    let secret_leak_src =
        fs::read_to_string(repo_root.join("examples/secret_leak_demo.ifm")).map_err(|e| {
            InfimumDiagnostic::new(
                "E040_FILE_READ",
                "showcase",
                format!("Cannot read examples/secret_leak_demo.ifm: {e}"),
                Span::zero(),
                "Ensure examples/secret_leak_demo.ifm exists",
            )
        })?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_ws = std::env::temp_dir().join(format!("infimum_showcase_build_{nanos}"));
    fs::create_dir_all(&temp_ws).map_err(|e| {
        InfimumDiagnostic::new(
            "E090_SHOWCASE_IO",
            "showcase",
            format!("Failed to create temp build workspace: {e}"),
            Span::zero(),
            "Check /tmp permissions",
        )
    })?;

    let result = (|| -> Result<ShowcaseDiagnosticsBundle, InfimumDiagnostic> {
        let mut store = GraphStore::open(&temp_ws)?;
        let payment_mod = parse_module(&payment_src)?;
        let ingested_nodes = store.ingest_module(&payment_mod)?;

        let wasm_dest = showcase_dir.join("checkout.wasm");
        let wasm_art = build_wasm_module(&store, &temp_ws, "checkout", Some(&wasm_dest))?;

        let stub_mod = parse_module(&anti_stub_src)?;
        let anti_stub_diagnostic = match store.verify_module(&stub_mod) {
            Err(diag) => diag,
            Ok(_) => {
                return Err(InfimumDiagnostic::new(
                    "E091_UNEXPECTED_VERIFY_PASS",
                    "showcase",
                    "Expected anti_stub_demo.ifm to be rejected by Z3",
                    Span::zero(),
                    "Verify anti_stub_demo.ifm contains a broken contract stub",
                ))
            }
        };

        let leak_mod = parse_module(&secret_leak_src)?;
        let secret_leak_diagnostic = match store.verify_module(&leak_mod) {
            Err(diag) => diag,
            Ok(_) => {
                return Err(InfimumDiagnostic::new(
                    "E091_UNEXPECTED_VERIFY_PASS",
                    "showcase",
                    "Expected secret_leak_demo.ifm to be rejected by taint checker",
                    Span::zero(),
                    "Verify secret_leak_demo.ifm contains an unauthorized secret sink",
                ))
            }
        };

        let bundle = ShowcaseDiagnosticsBundle {
            wasm_size_bytes: wasm_art.wasm_size_bytes,
            ingested_nodes,
            payment_source: payment_src,
            anti_stub_source: anti_stub_src,
            anti_stub_diagnostic,
            secret_leak_source: secret_leak_src,
            secret_leak_diagnostic,
        };

        let json = serde_json::to_string_pretty(&bundle).map_err(|e| {
            InfimumDiagnostic::new(
                "E090_SHOWCASE_IO",
                "showcase",
                format!("Failed to serialize showcase diagnostics: {e}"),
                Span::zero(),
                "Internal JSON serialization error",
            )
        })?;
        fs::write(showcase_dir.join("diagnostics.json"), json).map_err(|e| {
            InfimumDiagnostic::new(
                "E090_SHOWCASE_IO",
                "showcase",
                format!("Failed to write diagnostics.json: {e}"),
                Span::zero(),
                "Check write permissions in showcase/",
            )
        })?;

        Ok(bundle)
    })();

    let _ = fs::remove_dir_all(&temp_ws);
    result
}

pub fn serve_showcase_http(showcase_dir: &Path, bind_addr: &str) -> Result<(), InfimumDiagnostic> {
    let listener = TcpListener::bind(bind_addr).map_err(|e| {
        InfimumDiagnostic::new(
            "E092_HTTP_BIND",
            "showcase",
            format!("Cannot bind HTTP server on '{bind_addr}': {e}"),
            Span::zero(),
            "Choose an available local port, e.g. 127.0.0.1:8787",
        )
    })?;

    println!("Infimum Showcase Server listening at http://{bind_addr}");
    println!("Serving static Wasm & Time-Travel inspector from {}", showcase_dir.display());

    for stream in listener.incoming().flatten() {
        let _ = handle_http_conn(stream, showcase_dir);
    }
    Ok(())
}

fn handle_http_conn(mut stream: std::net::TcpStream, showcase_dir: &Path) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");

    if method != "GET" && method != "HEAD" {
        return write_http_response(&mut stream, "405 Method Not Allowed", "text/plain", b"Method Not Allowed");
    }

    let clean_path = raw_path.split('?').next().unwrap_or("/");
    let (rel_file, content_type) = match clean_path {
        "/" | "/index.html" => ("index.html", "text/html; charset=utf-8"),
        "/styles.css" => ("styles.css", "text/css; charset=utf-8"),
        "/app.js" => ("app.js", "application/javascript; charset=utf-8"),
        "/checkout.wasm" => ("checkout.wasm", "application/wasm"),
        "/diagnostics.json" => ("diagnostics.json", "application/json; charset=utf-8"),
        _ => {
            return write_http_response(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"Not Found");
        }
    };

    let full = showcase_dir.join(rel_file);
    match fs::read(&full) {
        Ok(body) => {
            let payload = if method == "HEAD" { &[][..] } else { &body[..] };
            write_http_response_len(&mut stream, "200 OK", content_type, body.len(), payload)
        }
        Err(_) => write_http_response(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"Asset Not Built"),
    }
}

fn write_http_response(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_http_response_len(stream, status, content_type, body.len(), body)
}

fn write_http_response_len(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    content_len: usize,
    body: &[u8],
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {content_len}\r\n\
         Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'\r\n\
         X-Frame-Options: DENY\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Permissions-Policy: camera=(), geolocation=(), microphone=()\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(headers.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}
