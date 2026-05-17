//! v3+ a-3: end-to-end coverage for remote `#import "https://..."`.
//!
//! These tests drive the public facade (`ResolverChainLoader`,
//! `value_from_str` / `value_from_str_trusted`) so that the gate
//! between sandbox / trust posture and the remote resolver mount
//! stays observable. Network access is mocked through an in-process
//! TCP listener — no external host is contacted.

use relon::ResolverChainLoader;
use relon_analyzer::{LoadError, ModuleLoader};
use relon_evaluator::module::{
    ModuleResolver, RemoteHttpResolver, RemoteImportCache, StdModuleResolver,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Smallest possible HTTP/1.1 server that serves one fixed body on
/// every accepted connection. Counts connections so the test can
/// assert cache hit / miss patterns.
struct MockServer {
    addr: String,
    hits: Arc<AtomicUsize>,
    _join: thread::JoinHandle<()>,
}

impl MockServer {
    fn start(body: &'static str, status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("local_addr").to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let join = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let reason = match status {
                    200 => "OK",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            addr,
            hits,
            _join: join,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn temp_cache_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let suffix = format!(
        "relon-remote-import-facade-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    path.push(suffix);
    path
}

#[test]
fn sandboxed_loader_denies_remote_url() {
    let mut loader = ResolverChainLoader::sandboxed();
    let err = loader
        .load("https://example.com/util.relon", std::path::Path::new("."))
        .unwrap_err();
    match err {
        LoadError::AccessDenied(reason) => {
            assert!(
                reason.contains("--trust") || reason.contains("network"),
                "unexpected denial reason: {reason}"
            );
        }
        other => panic!("expected AccessDenied, got {other:?}"),
    }
}

#[test]
fn trusted_loader_fetches_remote_url_via_mock_server() {
    let server = MockServer::start("{ remote_value: 42 }", 200);
    let cache = RemoteImportCache::new(temp_cache_dir("trusted-fetch"));
    let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
        Arc::new(StdModuleResolver),
        Arc::new(RemoteHttpResolver::with_cache(cache).allow_insecure(true)),
    ];
    let mut loader = ResolverChainLoader::from_resolvers_with_remote(resolvers, true);
    let loaded = loader
        .load(&server.url("/util.relon"), std::path::Path::new("."))
        .expect("trusted fetch should succeed");
    assert_eq!(loaded.source, "{ remote_value: 42 }");
    assert_eq!(server.hit_count(), 1);
}

#[test]
fn trusted_loader_uses_cache_on_second_call() {
    let server = MockServer::start("{ cached: 1 }", 200);
    let cache_dir = temp_cache_dir("cache-hit");
    let cache = RemoteImportCache::new(&cache_dir);

    // Two independent loader instances sharing the same disk cache so
    // we observe inter-run cache survival, not just per-instance
    // memoisation.
    let url = server.url("/foo.relon");
    {
        let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
            Arc::new(StdModuleResolver),
            Arc::new(RemoteHttpResolver::with_cache(cache.clone()).allow_insecure(true)),
        ];
        let mut loader = ResolverChainLoader::from_resolvers_with_remote(resolvers, true);
        let _ = loader
            .load(&url, std::path::Path::new("."))
            .expect("warm cache");
    }
    assert_eq!(
        server.hit_count(),
        1,
        "first call should fetch over the network"
    );
    {
        let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
            Arc::new(StdModuleResolver),
            Arc::new(RemoteHttpResolver::with_cache(cache).allow_insecure(true)),
        ];
        let mut loader = ResolverChainLoader::from_resolvers_with_remote(resolvers, true);
        let _ = loader
            .load(&url, std::path::Path::new("."))
            .expect("cache hit");
    }
    assert_eq!(
        server.hit_count(),
        1,
        "second call should be served from cache"
    );

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[test]
fn trusted_loader_surfaces_5xx_as_load_error_other() {
    let server = MockServer::start("oops", 500);
    let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
        Arc::new(StdModuleResolver),
        Arc::new(
            RemoteHttpResolver::new()
                .without_cache()
                .allow_insecure(true),
        ),
    ];
    let mut loader = ResolverChainLoader::from_resolvers_with_remote(resolvers, true);
    let err = loader
        .load(&server.url("/broken.relon"), std::path::Path::new("."))
        .unwrap_err();
    match err {
        LoadError::Other(msg) => {
            assert!(msg.contains("500"), "expected 500 in message, got {msg}");
        }
        other => panic!("expected LoadError::Other, got {other:?}"),
    }
}

#[test]
fn trusted_loader_surfaces_dns_failure_as_load_error_other() {
    // RFC 6761 reserves `.invalid` for guaranteed-to-fail DNS, so the
    // test does not depend on the host's actual network configuration.
    let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
        Arc::new(StdModuleResolver),
        Arc::new(RemoteHttpResolver::new().without_cache()),
    ];
    let mut loader = ResolverChainLoader::from_resolvers_with_remote(resolvers, true);
    let err = loader
        .load(
            "https://nonexistent.invalid/foo.relon",
            std::path::Path::new("."),
        )
        .unwrap_err();
    assert!(matches!(err, LoadError::Other(_)));
}

#[test]
fn facade_value_from_str_denies_remote_url_by_default() {
    // Sandboxed entry point: `value_from_str` runs without --trust,
    // so a remote `#import` must surface as a workspace-level error
    // (specifically an AccessDenied → ModuleNotFound shape that
    // carries the "use --trust" hint in the help text).
    let source = r#"#import lib from "https://example.com/util.relon"
{ x: lib.value }"#;
    let err = relon::value_from_str(source).unwrap_err();
    match err {
        relon::Error::AnalyzeWorkspace { workspace, .. } => {
            assert!(
                !workspace.is_empty(),
                "expected at least one workspace-level diagnostic"
            );
            let rendered: Vec<String> = workspace.iter().map(|d| d.to_string()).collect();
            let combined = rendered.join("\n");
            assert!(
                combined.contains("https://example.com/util.relon"),
                "diagnostic should mention the URL, got: {combined}"
            );
        }
        other => panic!("expected AnalyzeWorkspace, got {other:?}"),
    }
}

#[test]
fn from_resolvers_default_keeps_sandbox_short_circuit() {
    // A custom chain that *happens* to include the RemoteHttpResolver
    // but is constructed via the non-remote-aware `from_resolvers`
    // factory must still trip the sandboxed gate, because the host
    // never told the loader the chain answers URLs. This protects
    // against subtle host-side mistakes where the network resolver
    // is on the chain but the operator forgot to flip --trust.
    let resolvers: Vec<Arc<dyn ModuleResolver>> = vec![
        Arc::new(StdModuleResolver),
        Arc::new(RemoteHttpResolver::new()),
    ];
    let mut loader = ResolverChainLoader::from_resolvers(resolvers);
    let err = loader
        .load("https://example.com/foo.relon", std::path::Path::new("."))
        .unwrap_err();
    assert!(
        matches!(err, LoadError::AccessDenied(_)),
        "expected AccessDenied, got {err:?}"
    );
}
