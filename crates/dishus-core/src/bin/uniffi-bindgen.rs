//! Kotlin binding generator: `cargo run -p dishus-core --features bindgen-cli --bin uniffi-bindgen -- generate --library <lib> --language kotlin --out-dir <dir>`
fn main() {
    uniffi::uniffi_bindgen_main()
}
