fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }

    // `codex app-server daemon` reads the version as the second word of
    // `codex --version`, so the revision goes after it.
    println!("cargo:rerun-if-env-changed=CODEX_FORK_REVISION");
    let version = env!("CARGO_PKG_VERSION");
    match std::env::var("CODEX_FORK_REVISION") {
        Ok(revision) if !revision.is_empty() => {
            println!("cargo:rustc-env=CODEX_CLI_VERSION={version} (r{revision})");
        }
        _ => println!("cargo:rustc-env=CODEX_CLI_VERSION={version}"),
    }
}
