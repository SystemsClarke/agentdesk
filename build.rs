//! Embeds the Python half (`python/`: the Slack bridge, the vault plugin, dictation and the original's own acceptance checks) in
//! the binary, so a self-updated binary brings its Python with it. The list is generated; `PYTHON_HASH` changes with any file.

use std::{fs, path::Path};

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, std::path::PathBuf)>) {
    let mut entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().map(|n| n == "__pycache__").unwrap_or(false) {
                continue;
            }
            walk(&p, root, out);
        } else if p.extension().map(|x| x == "py" || x == "txt").unwrap_or(false) {
            out.push((p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), p));
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    println!("cargo:rerun-if-changed=python");
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut code = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        for b in rel.bytes().chain(fs::read(path).unwrap_or_default()) {
            hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
        }
        code.push_str(&format!("    ({rel:?}, include_bytes!({:?})),\n", path.to_string_lossy()));
    }
    code.push_str("];\n");
    code.push_str(&format!("pub const HASH: &str = \"{hash:016x}\";\n"));
    fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("python_files.rs"), code).unwrap();
}
