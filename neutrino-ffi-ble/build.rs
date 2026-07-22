// Copyright (c) 2026 Element Creations Ltd.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial.

//! Bake the exact upstream neutrino commit into the binary so `start_ble` can
//! log which neutrino it was built against at runtime. The source of truth is
//! the workspace lockfile: the resolved sha after `#` in the `git+` source of
//! the neutrino dependency (not the requested `rev`, which may be short or a
//! tag). Non-fatal — an unreadable/missing lockfile just yields "unknown".

use std::path::Path;

fn main() {
    // build.rs runs with CARGO_MANIFEST_DIR = this crate; the lockfile is the
    // workspace root's, one level up (this crate is `./neutrino-ffi-ble`).
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let lock = Path::new(&manifest).join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());

    let commit = std::fs::read_to_string(&lock)
        .ok()
        .and_then(|contents| neutrino_commit(&contents))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NEUTRINO_COMMIT={commit}");
}

/// The 40-hex resolved sha of the neutrino git dependency, parsed from a
/// Cargo.lock `source = "git+https://github.com/element-hq/neutrino...#<sha>"`
/// line. `None` if no such source is present.
fn neutrino_commit(lock: &str) -> Option<String> {
    lock.lines()
        .filter_map(|line| line.trim().strip_prefix("source = \""))
        .find(|src| src.contains("github.com/element-hq/neutrino") && src.contains('#'))
        .and_then(|src| src.rsplit('#').next())
        .map(|sha| sha.trim_end_matches('"').to_string())
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
}
