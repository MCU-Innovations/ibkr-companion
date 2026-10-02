fn main() {
    // Use the same Material component library registration as ECU Workbench.
    // It lets the UI import Slint's Material widgets through `@material`.
    let material = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("third_party/material/material.slint");
    println!("cargo:rerun-if-changed=third_party/material");
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
