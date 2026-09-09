fn main() {
    #[cfg(windows)]
    {
        cc::Build::new()
            .cpp(true)
            .file("src/gpu_bridge/gpu_bridge.cpp")
            .flag("/std:c++17")
            .flag("/EHsc")
            .warnings(true)
            .warnings_into_errors(true)
            .compile("rustconsole_gpu_bridge");
        build_wgc_helper();
        println!("cargo:rustc-link-lib=d3d11");
        println!("cargo:rustc-link-lib=d3d12");
        println!("cargo:rustc-link-lib=dxgi");
        println!("cargo:rustc-link-lib=d3dcompiler");
        println!("cargo:rustc-link-lib=runtimeobject");
        println!("cargo:rustc-link-lib=windowsapp");
        println!("cargo:rerun-if-changed=src/gpu_bridge/gpu_bridge.cpp");
        println!("cargo:rerun-if-changed=src/gpu_bridge/gpu_bridge.h");
        println!("cargo:rerun-if-changed=src/gpu_bridge/wgc_helper_main.cpp");
    }
}

#[cfg(windows)]
fn build_wgc_helper() {
    use std::env;
    use std::path::PathBuf;

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"))
        .join("rustconsole-wgc-helper.exe");
    let tool = cc::Build::new().cpp(true).get_compiler();
    let mut command = tool.to_command();
    command.args([
        "src/gpu_bridge/wgc_helper_main.cpp",
        "src/gpu_bridge/gpu_bridge.cpp",
        "/nologo",
        "/std:c++17",
        "/EHsc",
        "/W4",
        "/WX",
        "/DUNICODE",
        "/D_UNICODE",
    ]);
    command.arg(format!("/Fe:{}", output.display()));
    command.args([
        "/link",
        "/SUBSYSTEM:WINDOWS",
        "d3d11.lib",
        "d3d12.lib",
        "dxgi.lib",
        "d3dcompiler.lib",
        "runtimeobject.lib",
        "windowsapp.lib",
        "shell32.lib",
        "user32.lib",
    ]);
    let status = command.status().expect("run the WGC helper C++ compiler");
    assert!(status.success(), "WGC helper C++ compilation failed");
    println!(
        "cargo:rustc-env=RUSTCONSOLE_WGC_HELPER={}",
        output.display()
    );
}
