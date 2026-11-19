//! 一致性向量的全集：`spec/conformance/vectors` 下实际存在的每个向量文件，要么有本 SDK 的测试回放，要么显式豁免并写明理由。
//!
//! 规范新增向量文件时这里失败，不会因为没有测试读它而静默通过；规范删掉列出的文件时同样失败。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 本 SDK 的测试回放的向量文件（相对 `spec/conformance/vectors/`，以 `/` 分隔），行尾注释是回放它的测试。
///
/// `plugin/` 下的在打开 feature `plugin` 时回放；新增回放时加进来，不再回放时删掉。
const REPLAYED: [&str; 19] = [
    "manifest/manifests.json",                 // tests/manifest.rs
    "operations/payload-reasons.json",         // tests/operation.rs
    "pipeline/ordering.json",                  // tests/pipeline.rs
    "plugin/callback-status.json",             // tests/plugin.rs、src/plugin/host.rs
    "plugin/deadlines.json",                   // src/plugin/time.rs
    "plugin/error-reading.json",               // src/plugin/convert.rs
    "plugin/handshake-lines.json",             // tests/plugin.rs
    "plugin/host-addresses.json",              // tests/plugin.rs
    "plugin/plugin-messages.json",             // src/plugin/catalog.rs、src/plugin/messages.rs
    "plugin/protocol-versions.json",           // tests/plugin.rs
    "primitives/baggage-keys.json",            // tests/primitives.rs
    "primitives/capability-requirements.json", // tests/manifest.rs
    "primitives/error-codes.json",             // tests/primitives.rs
    "primitives/json-text.json",               // tests/primitives.rs
    "primitives/kernel-messages.json",         // tests/common（primitives、invocation、manifest、operation）
    "primitives/lifecycle-transitions.json",   // tests/primitives.rs
    "primitives/message-templates.json",       // tests/primitives.rs
    "primitives/trace-context.json",           // tests/invocation.rs
    "primitives/wire-encoding.json",           // tests/primitives.rs
];

/// 显式豁免的向量文件：文件与不回放的理由。
const EXEMPTED: [(&str, &str); 3] = [
    ("plugin/wasm-exports.json", "Wasm ABI exports are checked by hosts; this SDK has no Wasm plugin runtime."),
    ("provider/caching-distributed.json", "This SDK does not provide the caching.distributed@1 remote provider."),
    ("security/webauthn.json", "WebAuthn verification is host-side (RFC-0008); this SDK does not verify credentials."),
];

/// 递归收集 `directory` 下的 `.json` 文件，路径相对 `root`、以 `/` 分隔。
fn collect_json_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
    let entries = std::fs::read_dir(directory).unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display())).path();
        if path.is_dir() {
            collect_json_files(root, &path, files);
        } else if path.extension().is_some_and(|extension| extension == "json") {
            // 按路径组成部分拼接，各平台都以 / 分隔，与常量里的写法相同
            let relative = path.strip_prefix(root).expect("the file is under the vectors directory");
            files.insert(relative.components().map(|component| component.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
        }
    }
}

#[test]
fn every_vector_file_is_replayed_or_exempted() {
    let root: PathBuf = [env!("CARGO_MANIFEST_DIR"), "spec", "conformance", "vectors"].iter().collect();
    assert!(root.is_dir(), "{} is missing; run `git submodule update --init`", root.display());
    let mut present = BTreeSet::new();
    collect_json_files(&root, &root, &mut present);

    let replayed: BTreeSet<&str> = REPLAYED.into_iter().collect();
    let exempted: BTreeSet<&str> = EXEMPTED.into_iter().map(|(file, _)| file).collect();
    let both: Vec<&str> = replayed.intersection(&exempted).copied().collect();
    assert!(both.is_empty(), "vector files both replayed and exempted: {}", both.join(", "));
    let listed: BTreeSet<&str> = replayed.union(&exempted).copied().collect();
    let missing: Vec<&str> = listed.iter().copied().filter(|file| !present.contains(*file)).collect();
    assert!(missing.is_empty(), "listed vector files that the spec does not have: {}", missing.join(", "));
    let unlisted: Vec<&str> = present.iter().map(String::as_str).filter(|file| !listed.contains(file)).collect();
    assert!(unlisted.is_empty(), "spec vector files neither replayed nor exempted: {}", unlisted.join(", "));
}
