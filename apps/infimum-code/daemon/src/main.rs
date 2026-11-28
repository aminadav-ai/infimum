//! `infimum-agentd` — Privileged local developer daemon for Infimum Code.
//!
//! Runs completely outside `infimum-web` (zero GUI/GPU dependencies), listens strictly
//! on `127.0.0.1:<port>`, and authenticates every request with a session Bearer token.

mod agent;
mod core_loader;
mod http_server;
mod policy;
mod studio_abi;

use crate::http_server::ServerContext;
use crate::policy::CapabilityPolicy;
use std::env;
use std::fs::File;
use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn generate_random_token() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        // Fallback entropy if /dev/urandom is unavailable
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((now >> ((i % 16) * 8)) ^ (i as u128 * 31)) as u8;
        }
    }
    let mut out = String::with_capacity(64);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn main() {
    let mut port: u16 = 0; // 0 = OS picks an ephemeral free port on 127.0.0.1
    let mut token: Option<String> = env::var("INFIMUM_AGENT_TOKEN").ok().filter(|s| !s.is_empty());
    let mut policy = CapabilityPolicy::default();

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--port=") {
            port = v.parse::<u16>().unwrap_or(0);
        } else if a == "--port" && i + 1 < args.len() {
            i += 1;
            port = args[i].parse::<u16>().unwrap_or(0);
        } else if let Some(v) = a.strip_prefix("--token=") {
            token = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--allow-fs=") {
            policy.allowed_fs_roots.push(PathBuf::from(v));
        } else if a == "--allow-user-workspaces" {
            policy.allow_user_workspaces = true;
        } else if a == "--allow-proc" {
            policy.allow_proc = true;
        } else if a == "--dev-mode" {
            policy.dev_mode = true;
        } else if let Some(v) = a.strip_prefix("--allow-net=") {
            for d in v.split(',') {
                let d = d.trim();
                if !d.is_empty() {
                    policy.allowed_net_domains.insert(d.to_string());
                }
            }
        }
        i += 1;
    }

    let token = token.unwrap_or_else(generate_random_token);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("infimum-agentd failed to bind 127.0.0.1:{port}: {e}"));
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);

    // Machine-readable readiness line for the `infimum-code` launcher.
    // Note: token is intentionally NOT printed if supplied via INFIMUM_AGENT_TOKEN.
    println!("INFIMUM_AGENTD_READY port={bound_port}");

    let ctx = Arc::new(ServerContext {
        policy,
        token,
        shutdown: AtomicBool::new(false),
    });
    http_server::serve(listener, ctx);
}
