//! Record the compiler that built this binary so a bench report can say
//! which rustc produced the numbers (the same way fleetcode's results carry
//! the clang version).
use std::process::Command;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "rustc (unknown)".into());
    println!("cargo:rustc-env=C0MPUTE_RUSTC_VERSION={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");
}
