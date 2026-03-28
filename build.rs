use std::process::Command;

fn main() {
    // Embed short git hash at build time
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .unwrap_or_default();

    println!("cargo:rustc-env=GIT_HASH={}", hash.trim());

    // Re-run if HEAD changes (new commit)
    println!("cargo:rerun-if-changed=.git/HEAD");
}
