use std::{
    env, fs,
    path::{Path, PathBuf},
};
fn visit(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read built web assets") {
        let path = entry.expect("web asset entry").path();
        let kind = fs::symlink_metadata(&path)
            .expect("web asset metadata")
            .file_type();
        assert!(!kind.is_symlink(), "web assets must not contain symlinks");
        if kind.is_dir() {
            visit(root, &path, files);
        } else if kind.is_file() {
            assert!(path.starts_with(root));
            files.push(path);
        }
    }
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dist/web");
    let mut source = String::from("pub static WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    if env::var_os("CARGO_FEATURE_EMBEDDED_WEB").is_some() {
        let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("dist/web");
        assert!(
            root.join("index.html").is_file(),
            "Run npm run build before building with --features embedded-web"
        );
        let mut files = Vec::new();
        visit(&root, &root, &mut files);
        files.sort();
        for path in files {
            let name = format!(
                "/{}",
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            );
            source.push_str(&format!(
                "({name:?}, include_bytes!({:?})),\n",
                path.to_string_lossy()
            ));
        }
    }
    source.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("web_assets.rs"),
        source,
    )
    .unwrap();
}
