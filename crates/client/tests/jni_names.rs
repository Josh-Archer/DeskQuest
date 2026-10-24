//! The Android activities' `native` methods resolve by JNI symbol name. If a symbol does
//! not match the Java package, class and method exactly, the app links but crashes with
//! `UnsatisfiedLinkError` the first time the method is called (as the home panel did
//! after `com_linuxquest` became `com_linux_quest`). This checks every `native`
//! declaration in the Java sources against the Rust exports.

use std::path::Path;

/// JNI name mangling for one Java identifier segment: `_` becomes `_1`.
fn mangle(segment: &str) -> String {
    segment.replace('_', "_1")
}

#[test]
fn every_java_native_method_has_a_matching_rust_export() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let java_dir = root.join("android/src/com/linuxquest/client");
    let rust_sources: String = walk(&root.join("src"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    let mut checked = 0;
    for java in walk(&java_dir)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "java"))
    {
        let source = std::fs::read_to_string(&java).unwrap();
        let package = source
            .lines()
            .find_map(|l| l.trim().strip_prefix("package "))
            .map(|p| p.trim_end_matches(';').trim().to_string())
            .expect("package declaration");
        let class = java.file_stem().unwrap().to_str().unwrap();
        for line in source.lines().filter(|l| l.contains(" native ")) {
            let before_paren = line.split('(').next().unwrap();
            let method = before_paren.split_whitespace().last().unwrap();
            let symbol = format!(
                "Java_{}_{}_{}",
                package.split('.').map(mangle).collect::<Vec<_>>().join("_"),
                mangle(class),
                mangle(method)
            );
            assert!(
                rust_sources.contains(&format!("fn {symbol}(")),
                "{}: `{method}` needs a Rust export named {symbol}",
                java.display()
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no native methods found to check");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}
