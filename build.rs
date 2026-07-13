// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=intf.h");
    println!("cargo:rerun-if-changed=main.bpf.c");
    println!("cargo:rerun-if-changed=vendor/scx_rustland_core/assets/bpf/intf.h");
    println!("cargo:rerun-if-changed=vendor/scx_rustland_core/assets/bpf/main.bpf.c");
    println!("cargo:rerun-if-changed=vendor/scx_rustland_core/assets/bpf.rs");

    scx_rustland_core::RustLandBuilder::new()
        .unwrap()
        .build()
        .unwrap();
}
