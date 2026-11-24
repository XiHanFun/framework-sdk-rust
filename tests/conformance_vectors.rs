//! 按认领的一致性档核对向量（RFC-0024「一致性档」）：档表 `spec/conformance/profiles.json` 里本 SDK 认领的档要求的每个向量，
//! 要么有本 SDK 的测试回放，要么列为缺口并写明理由；回放的向量在规范里都存在。
//!
//! 规范在认领的档里新增向量时这里失败，不会因为没有测试读它而静默通过；回放认领的档以外的向量是允许的。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 本 SDK 认领的一致性档；档表里其他档的向量不归本 SDK。
const CLAIMED_PROFILES: [&str; 2] = ["core", "plugin"];

/// 本 SDK 的测试回放的向量文件（相对 `spec/conformance/vectors/`，以 `/` 分隔），行尾注释是回放它的测试。
///
/// `plugin/` 下的在打开 feature `plugin` 时回放；新增回放时加进来，不再回放时删掉。
const REPLAYED: [&str; 20] = [
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
    "primitives/operation-ids.json",           // tests/operation.rs
    "primitives/trace-context.json",           // tests/invocation.rs
    "primitives/wire-encoding.json",           // tests/primitives.rs
];

/// 缺口：认领的档要求、本 SDK 却没有回放的向量文件与不回放的理由。
///
/// RFC-0024「实现钉规范」第 3 条要求 README 的「状态」一节逐条写出同样的缺口；目前没有。
const GAPS: [(&str, &str); 0] = [];

/// 档表 `profiles.json`，只取用到的成员。
#[derive(Deserialize)]
struct ProfileTable {
    /// 全部档。
    profiles: Vec<Profile>,
}

/// 档表里的一个档。
#[derive(Deserialize)]
struct Profile {
    /// 档名。
    id: String,
    /// 认领这个档必须回放的向量，相对 `conformance/vectors`。
    vectors: Vec<String>,
}

#[test]
fn every_vector_of_the_claimed_profiles_is_replayed_or_a_gap() {
    let conformance: PathBuf = [env!("CARGO_MANIFEST_DIR"), "spec", "conformance"].iter().collect();
    let path = conformance.join("profiles.json");
    let data = std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}; run `git submodule update --init`", path.display()));
    let table: ProfileTable = serde_json::from_slice(&data).unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()));

    let undefined: Vec<&str> = CLAIMED_PROFILES.into_iter().filter(|claimed| !table.profiles.iter().any(|profile| profile.id == *claimed)).collect();
    assert!(undefined.is_empty(), "claimed profiles that the spec does not define: {}", undefined.join(", "));
    // 一个向量可以属于多个档，认领的档要求的向量取并集
    let required: BTreeSet<&str> = table
        .profiles
        .iter()
        .filter(|profile| CLAIMED_PROFILES.contains(&profile.id.as_str()))
        .flat_map(|profile| profile.vectors.iter().map(String::as_str))
        .collect();
    let replayed: BTreeSet<&str> = REPLAYED.into_iter().collect();
    let gaps: BTreeSet<&str> = GAPS.into_iter().map(|(file, _)| file).collect();
    let uncovered: Vec<&str> = required.iter().copied().filter(|file| !replayed.contains(file) && !gaps.contains(file)).collect();
    assert!(uncovered.is_empty(), "vector files of the claimed profiles that are neither replayed nor listed as gaps: {}", uncovered.join(", "));
    let outside: Vec<&str> = gaps.difference(&required).copied().collect();
    assert!(outside.is_empty(), "gaps outside the claimed profiles: {}", outside.join(", "));
    let replayed_gaps: Vec<&str> = gaps.intersection(&replayed).copied().collect();
    assert!(replayed_gaps.is_empty(), "gaps that are replayed; remove them from gaps: {}", replayed_gaps.join(", "));
    let vectors = conformance.join("vectors");
    let missing: Vec<&str> = replayed.iter().copied().filter(|file| !vector_path(&vectors, file).is_file()).collect();
    assert!(missing.is_empty(), "replayed vector files that the spec does not have: {}", missing.join(", "));
}

/// 以 `/` 分隔的向量路径在本平台上的位置。
fn vector_path(vectors: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(vectors.to_path_buf(), |path, part| path.join(part))
}
