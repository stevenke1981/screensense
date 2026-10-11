fn main() {
    if let Err(error) = slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-light".into()),
    ) {
        eprintln!("cannot compile ScreenSense UI: {error}");
        std::process::exit(1);
    }
}
