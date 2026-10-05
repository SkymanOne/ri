//! Deflates the committed `yapi-js.wasm` into `OUT_DIR`, so the binary embeds
//! the runtime at about a third of its size.

use std::io::Write as _;
use std::path::Path;

fn main() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("yapi-js.wasm");
    println!("cargo:rerun-if-changed={}", source.display());
    let wasm =
        std::fs::read(&source).expect("yapi-js.wasm; build it with `cargo xtask js-runtime`");
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&wasm).expect("deflate into memory");
    let deflated = encoder.finish().expect("deflate into memory");
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("yapi-js.wasm.deflate");
    std::fs::write(out, deflated).expect("write the deflated runtime");
}
