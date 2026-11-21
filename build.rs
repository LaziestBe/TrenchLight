// Windows resources: the icon Explorer shows and the version block the
// Properties dialog reads. Both binaries of this package get them.
//
// rc.exe from the Windows SDK does the work. winres finds it through the
// registry; if the SDK is missing the build fails here rather than shipping an
// exe with a blank icon.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let packed = (parts.next().unwrap_or(0) << 48)
        | (parts.next().unwrap_or(0) << 32)
        | (parts.next().unwrap_or(0) << 16)
        | parts.next().unwrap_or(0);

    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "Trenchlight");
    res.set("FileDescription", "Trenchlight - a Minecraft launcher");
    res.set(
        "LegalCopyright",
        "MIT licensed. Not affiliated with Mojang or Microsoft.",
    );
    res.set("OriginalFilename", "trenchlight.exe");
    res.set_version_info(winres::VersionInfo::FILEVERSION, packed);
    res.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed);

    res.compile()
        .expect("compiling Windows resources (rc.exe from the Windows SDK is required)");
}
