// SPDX-License-Identifier: GPL-3.0-or-later
//! Embeds `Info.plist` into the launcher binary (`__TEXT,__info_plist`), the
//! standard way to give an unbundled Mach-O executable a bundle identifier
//! and `LSUIElement` (see `src/appkit.rs`).

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let plist = std::path::Path::new(&manifest_dir).join("Info.plist");
    println!("cargo::rerun-if-changed={}", plist.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo::rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
