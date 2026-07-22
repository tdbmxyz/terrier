//! Embed the git commit for /api/health and the About tab. Nix builds
//! (no .git in the sandbox) pass GIT_COMMIT via the flake; dev/justfile
//! builds ask git; anything else says "unknown".

fn main() {
    println!("cargo:rerun-if-env-changed=GIT_COMMIT");
    let commit = std::env::var("GIT_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short=9", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TERRIER_COMMIT={commit}");
}
