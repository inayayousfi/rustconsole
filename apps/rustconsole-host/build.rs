fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        tauri_winres::WindowsResource::new()
            .set_icon("../../assets/icon.ico")
            .compile()
            .expect("failed to embed the Rust Console icon");
    }
}
