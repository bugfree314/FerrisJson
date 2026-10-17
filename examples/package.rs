//! Build and package the current platform: Windows/macOS GUI or Linux TUI.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    if let Err(error) = package() {
        eprintln!("Packaging failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn package() -> Result<(), Box<dyn std::error::Error>> {
    Err(
        "Packaging supports Windows, macOS and Linux; use cargo build --release on other systems"
            .into(),
    )
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn package() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = root.join("target");
    let release = target.join("release");
    let output = release.join("bundle");
    let mut build = Command::new("cargo");
    build
        .current_dir(&root)
        .args([
            "build",
            "--release",
            "--locked",
            "--bin",
            "FerrisJson",
            "--target-dir",
        ])
        .arg(&target);
    // Explicitly use the host target, even if a Cargo config specifies a default target.
    let host = host_target()?;
    build.args(["--target", &host]);
    #[cfg(target_os = "macos")]
    build.env("MACOSX_DEPLOYMENT_TARGET", "11.0");
    run(&mut build)?;
    let binary_dir = target.join(&host).join("release");
    fs::create_dir_all(&output)?;

    #[cfg(target_os = "windows")]
    package_windows(&root, &binary_dir, &output)?;
    #[cfg(target_os = "macos")]
    package_macos(&root, &binary_dir, &output)?;
    #[cfg(target_os = "linux")]
    package_linux(&root, &binary_dir, &output)?;
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn host_target() -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("rustc").arg("-vV").output()?;
    if !output.status.success() {
        return Err("rustc -vV failed".into());
    }
    String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| "Cannot determine the Rust host target".into())
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn run(command: &mut Command) -> Result<(), Box<dyn std::error::Error>> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} exited with {status}").into());
    }
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn distribution_files(root: &Path, destination: &Path) -> std::io::Result<()> {
    for name in ["LICENSE", "README.md"] {
        fs::copy(root.join(name), destination.join(name))?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn package_windows(
    root: &Path,
    binary_dir: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let executable = output.join("FerrisJson.exe");
    fs::copy(binary_dir.join("FerrisJson.exe"), &executable)?;
    distribution_files(root, output)?;
    println!("Executable: {}", executable.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn package_macos(
    root: &Path,
    binary_dir: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = output.join("FerrisJson.app");
    let contents = app.join("Contents");
    let executable = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&executable)?;
    fs::create_dir_all(&resources)?;
    fs::copy(binary_dir.join("FerrisJson"), executable.join("FerrisJson"))?;
    distribution_files(root, &resources)?;
    let iconset = output.join("FerrisJson.iconset");
    fs::create_dir_all(&iconset)?;
    for size in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let pixels = (size * scale).to_string();
            let suffix = if scale == 2 { "@2x" } else { "" };
            run(Command::new("sips")
                .args(["-z", &pixels, &pixels])
                .arg(root.join("assets/FerrisJson.png"))
                .arg("--out")
                .arg(iconset.join(format!("icon_{size}x{size}{suffix}.png"))))?;
        }
    }
    run(Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .arg("-o")
        .arg(resources.join("FerrisJson.icns")))?;
    let version = env!("CARGO_PKG_VERSION");
    fs::write(
        contents.join("Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleName</key><string>FerrisJson</string>
    <key>CFBundleDisplayName</key><string>FerrisJson</string>
    <key>CFBundleIdentifier</key><string>io.ferrisjson.FerrisJson</string>
    <key>CFBundleExecutable</key><string>FerrisJson</string>
    <key>CFBundleIconFile</key><string>FerrisJson.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
"#
        ),
    )?;
    run(Command::new("plutil")
        .arg("-lint")
        .arg(contents.join("Info.plist")))?;
    // Ad-hoc signing needs no Apple account; public distribution can re-sign this bundle.
    run(Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app))?;
    run(Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(&app))?;
    println!("Application: {}", app.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn package_linux(
    root: &Path,
    binary_dir: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let executable = output.join("FerrisJson");
    fs::copy(binary_dir.join("FerrisJson"), &executable)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    distribution_files(root, output)?;
    println!("Linux TUI executable: {}", executable.display());
    Ok(())
}
