fn main() {
    let source = "src/control_runtime/real8_pack.c";
    println!("cargo:rerun-if-changed={source}");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm")
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
    {
        cc::Build::new().file(source).flag("-mcpu=cortex-a9").flag("-mfpu=neon").compile("real8_pack");
    }
}
