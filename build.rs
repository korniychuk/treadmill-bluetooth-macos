//! Build script: embed a macOS `Info.plist` into the binary.
//!
//! On macOS, CoreBluetooth reads the Bluetooth usage description and bundle
//! identifier from the running process. A bare Cargo binary has neither, so TCC
//! attributes the Bluetooth grant to the *terminal* instead of this app. By
//! stuffing the plist into the `__TEXT,__info_plist` Mach-O section, even the
//! plain `target/debug` binary carries a stable bundle id + usage string, so a
//! (code-signed) run gets its own TCC identity and its own one-time prompt.

fn main() {
    // `CARGO_CFG_TARGET_OS` reflects the *target*, not the build-script host —
    // correct even under cross-compilation. Only macOS understands the section.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let plist = format!("{manifest_dir}/macos/Info.plist");

    println!("cargo:rerun-if-changed={plist}");
    println!("cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{plist}");
}
