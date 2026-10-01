fn main() {
    slint_build::compile("ui/app.slint").expect("compile Slint UI");

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
