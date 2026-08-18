//! Embeds the application manifest, whose detached console allocation policy (Windows 11 24H2+)
//! keeps the logon autostart from opening a console window while the binary stays a console app
//! that shells wait for.

fn main() {
    // link.exe flags: a gnu-flavour linker would read `/MANIFEST:EMBED` as an input file name.
    // Refused rather than skipped, because a build without the manifest works until logon, where
    // the autostart opens a console window on every start.
    assert!(
        std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc"),
        "the MSVC toolchain is required: the console allocation policy manifest is embedded \
         through link.exe flags"
    );
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contextwitness.manifest");
    println!("cargo:rerun-if-changed=contextwitness.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
