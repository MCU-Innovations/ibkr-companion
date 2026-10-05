fn main() {
    // Use Material from the same official upstream source Cargo resolved for Slint.
    let metadata = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .args(["metadata", "--format-version=1", "--locked", "--offline"])
        .arg("--filter-platform")
        .arg(std::env::var("TARGET").unwrap())
        .output()
        .expect("read Cargo dependency metadata");
    assert!(
        metadata.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata.stdout).expect("parse Cargo metadata");
    let manifest = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "slint-build")
        .and_then(|package| package["manifest_path"].as_str())
        .expect("resolved slint-build manifest");
    let material = std::path::Path::new(manifest)
        .parent()
        .unwrap()
        .join("../../../ui-libraries/material/src/material.slint");
    println!(
        "cargo:rerun-if-changed={}",
        material.parent().unwrap().display()
    );
    assert!(
        material.exists(),
        "Slint Material library was not found: {}",
        material.display()
    );
    let library_paths = std::collections::HashMap::from([("material".to_string(), material)]);
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_library_paths(library_paths),
    )
    .expect("compile Slint UI");

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=resources/IBKR.ico");
        let mut resource = winres::WindowsResource::new();
        resource
            .set_icon("resources/IBKR.ico")
            .set("FileDescription", "IBKR Companion")
            .set("ProductName", "IBKR Companion");
        resource.compile().expect("embed Windows app icon");
    }
}
