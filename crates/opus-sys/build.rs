//! Builds the vendored libopus (`third_party/opus`, float build) with `cc`.
//! The source lists come from upstream's own `*_sources.mk` files, so an
//! upstream update only needs the directory replaced.

use std::path::{Path, PathBuf};

/// Files listed under `var = \` ... in an automake fragment.
fn sources(mk: &Path, var: &str) -> Vec<String> {
    let text = std::fs::read_to_string(mk).unwrap_or_else(|e| panic!("{}: {e}", mk.display()));
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if !inside {
            if let Some(rest) = line.strip_prefix(var) {
                if rest.trim_start().starts_with('=') {
                    out.extend(files(rest.trim_start()[1..].trim()));
                    inside = line.ends_with('\\');
                }
            }
            continue;
        }
        out.extend(files(line));
        inside = line.ends_with('\\');
    }
    out
}

fn files(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split_whitespace()
        .filter(|w| *w != "\\" && w.ends_with(".c"))
        .map(str::to_string)
}

fn main() {
    let root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../third_party/opus");
    let mut files = Vec::new();
    for (mk, var) in [
        ("celt_sources.mk", "CELT_SOURCES"),
        ("silk_sources.mk", "SILK_SOURCES"),
        ("silk_sources.mk", "SILK_SOURCES_FLOAT"),
        ("opus_sources.mk", "OPUS_SOURCES"),
        ("opus_sources.mk", "OPUS_SOURCES_FLOAT"),
    ] {
        let mkp = root.join(mk);
        println!("cargo:rerun-if-changed={}", mkp.display());
        let v = sources(&mkp, var);
        assert!(!v.is_empty(), "{var} empty in {mk}");
        files.extend(v);
    }
    let mut b = cc::Build::new();
    b.include(root.join("include"))
        .include(root.join("celt"))
        .include(root.join("silk"))
        .include(root.join("silk/float"))
        .include(&root)
        .define("OPUS_BUILD", None)
        .define("USE_ALLOCA", None)
        .define("PACKAGE_VERSION", "\"1.3-vendored\"")
        .warnings(false);
    if !b.get_compiler().is_like_msvc() {
        b.define("HAVE_LRINTF", None)
            .flag_if_supported("-fvisibility=hidden");
    }
    for f in &files {
        b.file(root.join(f));
    }
    b.compile("opus");
}
