// Embeds the Russenger icon and version info into the Windows executables.
/// Table of the emoji pictures in assets/emoji, compiled into the program (the picker and the
/// reaction buttons then never need the network).
fn embed_emoji() {
    use std::fmt::Write;
    let dir = std::path::Path::new("../assets/emoji").canonicalize().expect("assets/emoji");
    println!("cargo:rerun-if-changed=../assets/emoji");
    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().filter_map(|e| e.file_name().to_str().map(String::from)).filter(|n| n.ends_with(".png")).collect();
    names.sort();
    let mut code = String::from("pub fn get(code: &str) -> Option<&'static [u8]> {\n    match code {\n");
    for n in &names {
        let stem = n.trim_end_matches(".png");
        let path = dir.join(n).to_string_lossy().replace('\\', "/");
        writeln!(code, "        {stem:?} => Some(include_bytes!({path:?})),").unwrap();
    }
    code.push_str("        _ => None,\n    }\n}\n");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("emoji_embedded.rs");
    std::fs::write(out, code).unwrap();
}

fn main() {
    embed_emoji();
    println!("cargo:rerun-if-changed=../assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/icon.ico");
        res.set("ProductName", "Russenger");
        res.set("FileDescription", "Russenger - Messenger client for Windows");
        res.set("LegalCopyright", "AGPL-3.0");
        if let Err(e) = res.compile() {
            // not fatal: the app still builds, just without the embedded icon
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
}
