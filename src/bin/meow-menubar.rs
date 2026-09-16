#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    meow::menubar::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("meow-menubar is supported on macOS only");
    std::process::exit(1);
}
