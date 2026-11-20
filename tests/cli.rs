//! The binary starts and every subcommand from section 6 exists.

mod support;

use std::process::Command;

#[test]
fn every_subcommand_has_help() {
    for sub in ["serve", "valet", "check", "build", "attach", "status", "mcp", "composer"] {
        let out = Command::new(support::gllmh_bin()).args([sub, "--help"]).output().unwrap();
        assert!(out.status.success(), "gllmh {sub} --help failed: {}", String::from_utf8_lossy(&out.stderr));
    }
}
