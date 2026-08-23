fn main() {
    // Windows executables default to a 1 MiB stack. Rich help can recursively
    // walk nested MCP schemas deeply enough to exceed that, while Unix's
    // customary 8 MiB main-thread stack has ample headroom.
    #[cfg(all(windows, target_env = "msvc"))]
    println!("cargo::rustc-link-arg-bin=mcp-repl=/STACK:8388608");
}
