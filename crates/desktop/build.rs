use std::path::PathBuf;
use std::process::Command;

fn main() {
    let icons = "../../packaging/icons";
    let manifest = "resources.gresource.xml";
    println!("cargo:rerun-if-changed={manifest}");
    println!("cargo:rerun-if-changed=src/theme.css");
    println!("cargo:rerun-if-changed=src/theme-modern.css");
    println!("cargo:rerun-if-changed={icons}/hicolor/512x512/apps/org.deskquest.Desktop.png");

    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"))
        .join("deskquest.gresource");
    let status = Command::new("glib-compile-resources")
        .arg(manifest)
        .arg(format!("--sourcedir={icons}"))
        .arg("--sourcedir=.")
        .arg("--target")
        .arg(output)
        .status()
        .expect("glib-compile-resources is required; install libglib2.0-dev-bin");
    assert!(
        status.success(),
        "could not compile the desktop icon resource"
    );
}
