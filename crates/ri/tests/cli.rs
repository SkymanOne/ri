//! Command-line contract tests for the `ri` binary.

use std::process::Command;

#[test]
fn version_prints_bare_version() {
    for flag in ["--version", "-v"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ri"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag} failed");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}
