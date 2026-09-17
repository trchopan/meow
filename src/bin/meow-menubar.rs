#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    let _ = meow::logging::init_logging("menubar", true);
    meow::logging::install_panic_hook(true);
    meow::menubar::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("meow-menubar is supported on macOS only");
    std::process::exit(1);
}
