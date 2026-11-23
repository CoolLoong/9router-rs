use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(chatgpt_web_replay)");
    println!(
        "cargo:rustc-env=NINEROUTER_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo target triple")
    );
    println!("cargo:rerun-if-changed=dashboard/dist");
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest path"))
        .join("dashboard/dist");
    assert!(
        root.join("index.html").is_file(),
        "Build the dashboard first: npm --prefix dashboard ci && npm --prefix dashboard run build"
    );
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    write_assets(&root, &output.join("dashboard_assets.rs")).expect("embed built dashboard assets");
}

fn write_assets(root: &Path, output: &Path) -> io::Result<()> {
    let mut entries = Vec::new();
    collect_files(root, root, &mut entries)?;
    entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    let mut output = io::BufWriter::new(fs::File::create(output)?);
    writeln!(output, "const FILES: &[(&str, &[u8])] = &[")?;
    for (name, file) in entries {
        writeln!(output, "    ({name:?}, include_bytes!({:?})),", file)?;
    }
    writeln!(output, "];")?;
    output.flush()
}

fn collect_files(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<(String, PathBuf)>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if directory != root || entry.file_name() != "licenses" {
                collect_files(root, &file, entries)?;
            }
        } else if kind.is_file() {
            let name = file.strip_prefix(root).expect("dashboard asset path");
            entries.push((name.to_string_lossy().replace('\\', "/"), file));
        }
    }
    Ok(())
}
