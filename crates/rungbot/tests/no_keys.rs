//! The notify-only promise, enforced by the build rather than documented in a README.
//!
//! rungbot accepts no API key and contains no request-signing code. If anyone ever adds
//! some, this test fails before it can ship. It lives outside `src/` deliberately, so
//! its own list of forbidden strings is not part of what it scans.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_shipped_code_contains_no_signing_or_key_handling() {
    let root = workspace_root();
    let banned = [
        concat!("hm", "ac"),
        concat!("api_", "secret"),
        concat!("api", "Key"),
        concat!("X-MBX-", "APIKEY"),
        concat!("private_", "key"),
        concat!("signat", "ure"),
        concat!("KEY", "_SECRET"),
    ];

    let mut files = Vec::new();
    // Every crate the CLI links is scanned: all of it ships in the keyless binary.
    for c in [
        "crates/rungbot-core/src",
        "crates/rungbot-notify/src",
        "crates/rungbot/src",
        "crates/rungbot-backtest/src",
        "crates/rungbot-research/src",
    ] {
        rust_sources(&root.join(c), &mut files);
    }
    assert!(
        !files.is_empty(),
        "the scan found no sources, so it proves nothing"
    );

    let mut hits = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path)
            .expect("read source")
            .to_ascii_lowercase();
        for token in banned {
            if body.contains(&token.to_ascii_lowercase()) {
                hits.push(format!("{} mentions {token:?}", path.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "rungbot must hold no keys and sign no requests:\n  {}",
        hits.join("\n  ")
    );
}

#[test]
fn no_dependency_pulls_in_an_exchange_sdk() {
    let root = workspace_root();
    let manifests = [
        "crates/rungbot-core/Cargo.toml",
        "crates/rungbot-notify/Cargo.toml",
        "crates/rungbot/Cargo.toml",
        "crates/rungbot-backtest/Cargo.toml",
        "crates/rungbot-research/Cargo.toml",
    ];
    for m in manifests {
        let body = std::fs::read_to_string(root.join(m))
            .expect("read manifest")
            .to_lowercase();
        for banned in ["binance-rs", "ccxt", "openssl", "keyring"] {
            assert!(
                !body.contains(banned),
                "{m} depends on {banned:?}, which this project has no business needing"
            );
        }
    }
}

#[test]
fn the_notify_only_binary_does_not_depend_on_the_executor() {
    // Everything that can move money lives in rungbot-exec, behind its own binary.
    // If that crate ever becomes a dependency of the CLI, installing the ladder would
    // quietly install the ability to trade.
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("crates/rungbot/Cargo.toml"))
        .expect("read the rungbot manifest");
    assert!(
        !manifest.contains("rungbot-exec"),
        "the rungbot crate must not depend on rungbot-exec"
    );
    // The notifier is shared by both binaries, so it must not reach back into the
    // executor either, or the CLI would pull it in through the side door.
    let notify = std::fs::read_to_string(root.join("crates/rungbot-notify/Cargo.toml"))
        .expect("read the rungbot-notify manifest");
    assert!(
        !notify.contains("rungbot-exec"),
        "rungbot-notify must not depend on rungbot-exec"
    );

    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read the lockfile");
    assert!(
        lock.contains("rungbot-exec"),
        "the executor should still be in the workspace, just not wired into the CLI"
    );
}
