//! Build script for `vaern-cli`.
//!
//! Owns `VAERN_VERSION`, the string `--version` reports. It is the crate
//! version alone so the binary, the Cargo manifests and the npm package all
//! show the same release number; a git SHA suffix would make them disagree.

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    println!("cargo:rustc-env=VAERN_VERSION={version}");
    println!("cargo:rerun-if-changed=Cargo.toml");
}
