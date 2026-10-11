fn main() {
    println!("cargo:rerun-if-changed=src/updater.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/updater.m")
            .flag("-fobjc-arc")
            .compile("latch_updater");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }
}
