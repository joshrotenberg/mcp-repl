fn main() {
    link_gnu_atomic_runtime();
    // Windows executables default to a 1 MiB stack. Rich help can recursively
    // walk nested MCP schemas deeply enough to exceed that, while Unix's
    // customary 8 MiB main-thread stack has ample headroom.
    #[cfg(all(windows, target_env = "msvc"))]
    println!("cargo::rustc-link-arg-bin=mcp-repl=/STACK:8388608");
}

// GCC's outlined ARM atomics live in libgcc, which Rust's -nodefaultlibs
// omits on musl. Vendored libdbus uses those helpers. Keep this in the source
// package (not only CI) and use the selected target C compiler's runtime.
fn link_gnu_atomic_runtime() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("aarch64")
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("musl")
    {
        return;
    }
    let compiler = cc::Build::new().get_compiler();
    if !compiler.is_like_gnu() {
        return;
    }
    let output = compiler
        .to_command()
        .arg("-print-libgcc-file-name")
        .output()
        .expect("query target GCC runtime for vendored libdbus");
    assert!(
        output.status.success(),
        "target GCC could not locate libgcc"
    );
    let archive = String::from_utf8(output.stdout).expect("GCC runtime path must be UTF-8");
    let archive = std::path::Path::new(archive.trim());
    assert!(
        archive.is_file(),
        "target GCC's libgcc archive was not found"
    );
    let directory = archive.parent().expect("GCC runtime directory");
    println!("cargo::rustc-link-search=native={}", directory.display());
    // Rust schedules static/unspecified native libraries beside this crate,
    // before libdbus introduces the helper references. Use its late native
    // library pass, but name the exact archive: -l:libgcc.a still links GCC
    // statically and also propagates to downstream users of mcp_repl.
    println!("cargo::rustc-link-lib=dylib:+verbatim=libgcc.a");
}
