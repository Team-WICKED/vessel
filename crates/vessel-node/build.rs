//! Exports the target triple for `vesseld version`, which the remote
//! installer reads to confirm it installed the right build (ADR 0008).
//! Cargo gives build scripts `TARGET`; the compiled crate has no other way
//! to know the full triple (`std::env::consts` lacks the ABI, `musl` vs
//! `gnu`).

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_unset| "unknown".to_owned());
    println!("cargo:rustc-env=VESSEL_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
