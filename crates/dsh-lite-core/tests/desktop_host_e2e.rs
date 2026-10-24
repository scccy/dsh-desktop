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

use dsh_lite_core::desktop_host::{self, HostSpec};

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
    let home = std::env::var("HOME").expect("HOME");
    let project_dir = PathBuf::from(home)
        .join(".dsh")
        .join("profiles")
        .join("desktop");

    let spec = HostSpec {
        node: node.into(),
        script,
        dsh_package,
        project_dir,
        version: "0.1.0".to_owned(),
        env: Vec::new(),
    };

    let mut host = desktop_host::spawn(&spec, Duration::from_secs(30)).expect("desktop host ready");
    assert_eq!(host.ready().url.scheme(), "http");
    assert!(
        host.ready().url.as_str().contains("token="),
        "ready url must carry the one-shot token: {}",
        host.ready().url
    );
    assert!(host.ready().injections > 0, "expected index injections");
    assert!(host.pid() > 0);

    host.stop(Duration::from_secs(8));
}
