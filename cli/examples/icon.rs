//! `cargo run -p substrate --example icon -- <folder>...`: which logo each project folder yields.
fn main() {
    for f in std::env::args().skip(1) {
        match substrate_cli::icon::detect(&f) {
            Some(b) => {
                if let Ok(dir) = std::env::var("ICON_OUT") {
                    let _ = std::fs::write(format!("{dir}/{}.png", f.rsplit('/').next().unwrap_or("icon")), &b);
                }
                println!("{f}: {} bytes", b.len())
            }
            None => println!("{f}: none (lettered tile)"),
        }
    }
}
