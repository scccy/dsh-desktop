//! End-to-end check of the Rust side of the Desktop Host.
//!
//! Ignored by default because it needs a real dsh installation and Node on
//! PATH. Run it explicitly:
//!
//! ```sh
//! DSH_DESKTOP_E2E_DSH_PATH=/path/to/node_modules/@deepseek-ai/dsh \
//!   cargo test -p dsh-lite-core --test desktop_host_e2e -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use dsh_lite_core::desktop_host::{self, HOST_PROTOCOL_VERSION, HostSpec};
use dsh_lite_core::url_policy;

#[test]
#[ignore = "requires a real dsh install (DSH_DESKTOP_E2E_DSH_PATH)"]
fn spawns_the_real_desktop_host_and_stops_it() {
    let dsh_package = match std::env::var("DSH_DESKTOP_E2E_DSH_PATH") {
        Ok(value) => PathBuf::from(value),
        Err(_) => return,
    };
    let node = std::env::var("DSH_DESKTOP_E2E_NODE").unwrap_or_else(|_| "node".to_owned());
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("src-tauri")
        .join("host")
        .join("desktop-host.mjs");
    let project_dir = HostSpec::profile_dir_from_process_env().expect("a profile directory");
    assert!(
        project_dir.ends_with("profiles/desktop"),
        "the shell must use its own desktop profile, got {}",
        project_dir.display()
    );

    let spec = HostSpec {
        node: node.into(),
        script,
        dsh_package,
        project_dir,
        version: "0.1.0".to_owned(),
        env: Vec::new(),
    };

    // The Rust preflight and the launcher's own check must agree about this
    // machine's dsh; a real run that is refused here is the whole point.
    let gate = desktop_host::probe_version(&spec, Duration::from_secs(20)).expect("dsh --version");
    assert!(
        gate.is_supported(),
        "this dsh is refused: {}",
        gate.refusal().unwrap_or_default()
    );

    let mut host = desktop_host::spawn(&spec, Duration::from_secs(30)).expect("desktop host ready");
    let ready = host.ready();

    assert_eq!(ready.url.scheme(), "http");
    let token = url_policy::token_of(&ready.url).expect("the ready url carries the one-shot token");
    assert!(!token.is_empty());
    assert_eq!(ready.origin, url_policy::clean_url(&ready.url));
    assert!(url_policy::is_loopback_http(&ready.origin));
    assert!(url_policy::is_token_free(&ready.origin));
    assert!(url_policy::shares_target(&ready.url, &ready.origin));
    assert_eq!(ready.protocol, "http:");
    assert_eq!(ready.protocol_version, HOST_PROTOCOL_VERSION);

    // 13 rows on dsh 0.2.1-rc.2 (12 on 0.2.0-rc.2), all structured objects.
    assert!(ready.has_injection_rows(), "expected index injections");
    assert_eq!(ready.injections.len(), ready.injection_count);
    assert!(
        ready.injections.iter().all(|row| row.is_object()),
        "injection rows must be objects"
    );

    println!(
        "ready: origin={} protocol={} protocolVersion={} injections={} (token redacted)",
        ready.origin, ready.protocol, ready.protocol_version, ready.injection_count
    );
    assert!(host.pid() > 0);

    host.stop(Duration::from_secs(8));
}
