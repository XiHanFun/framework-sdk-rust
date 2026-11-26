//! 进程外插件的黑盒一致性（RFC-0017「一致性」、`spec/conformance/README.md`）：构建示例 echo，把它与清单放进临时目录，
//! 用规范子模块里的运行器 `xihan-conformance plugin` 跑全部用例，断言每个用例都通过、没有跳过。
//!
//! 运行器从 `spec/conformance` 以 `go run` 运行，版本就是锁定的规范；本机没有 Go 时测试失败，只有设了
//! `XIHAN_CONFORMANCE_REQUIRED=0` 才打印原因并提前返回（Rust 测试没有运行期的跳过，这样返回会记为通过）。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// 运行器的全部用例（`spec/conformance/README.md` 的用例表）。
const CASES: [&str; 15] = [
    "handshake",
    "refusal",
    "session",
    "lifecycle",
    "order",
    "unknown-operation",
    "declared-operations",
    "unavailable",
    "event",
    "event-envelope",
    "unsubscribed",
    "stream",
    "configuration",
    "stop-early",
    "stdin",
];

/// 运行器 `--json` 输出的一项。
#[derive(Debug, Deserialize)]
struct Outcome {
    case: String,
    passed: bool,
    skipped: bool,
    #[serde(default)]
    message: String,
}

#[test]
fn echo_passes_the_black_box_runner() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let conformance = root.join("spec").join("conformance");
    assert!(conformance.join("go.mod").is_file(), "{} is missing; run `git submodule update --init`", conformance.display());
    match Command::new("go").arg("version").output() {
        Ok(output) if output.status.success() => {}
        // 缺 Go 默认失败，否则门禁会静默变绿；确实要跳过时显式设 XIHAN_CONFORMANCE_REQUIRED=0
        _ if std::env::var("XIHAN_CONFORMANCE_REQUIRED").as_deref() == Ok("0") => {
            eprintln!("skipped because XIHAN_CONFORMANCE_REQUIRED=0: Go is not on PATH; the black-box runner of the spec needs it (https://go.dev/dl/)");
            return;
        }
        _ => panic!(
            "Go is not on PATH; the black-box runner of the spec needs it: install Go (https://go.dev/dl/) or set XIHAN_CONFORMANCE_REQUIRED=0 to skip this test"
        ),
    }

    let directory = plugin_directory(&build_echo(root));
    let output = Command::new("go")
        .current_dir(&conformance)
        .args(["run", "./cmd/xihan-conformance", "plugin", "--dir"])
        .arg(&directory)
        .args([
            "--json",
            "--quiet",
            "--config",
            r#"{"greeting":"hello"}"#,
            "--sample",
            r#"echoes.create={"text":"conformance"}"#,
            "--exclude",
            "echoes.crash",
            "--envelopes",
            "events.list",
        ])
        .output()
        .expect("the black-box runner could not be started");
    let _ = std::fs::remove_dir_all(&directory);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let outcomes: Vec<Outcome> = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "the runner did not print JSON ({error}); exit {:?}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    for outcome in &outcomes {
        eprintln!("{:<20} passed={} skipped={} {}", outcome.case, outcome.passed, outcome.skipped, outcome.message);
    }
    assert_eq!(outcomes.iter().map(|outcome| outcome.case.as_str()).collect::<Vec<_>>(), CASES);
    for outcome in &outcomes {
        assert!(outcome.passed && !outcome.skipped, "{outcome:?}");
    }
    assert!(output.status.success(), "the runner exited with {:?}", output.status.code());
}

/// 构建示例 echo，返回可执行文件的路径（取自 cargo 的构建消息，不猜目标目录的布局）；已经是最新的时 cargo 什么都不做。
///
/// 目标目录沿用环境与配置里的设置，与运行本测试的 `cargo test` 相同；本测试在 release 下运行时同样按 release 构建。
fn build_echo(root: &Path) -> PathBuf {
    let executable = std::env::current_exe().expect("the test executable has no path");
    let mut build = Command::new(env!("CARGO"));
    build.current_dir(root).args(["build", "--example", "echo", "--features", "plugin", "--message-format=json-render-diagnostics"]);
    if executable.ancestors().any(|directory| directory.file_name().is_some_and(|name| name == "release")) {
        build.arg("--release");
    }
    let output = build.stderr(std::process::Stdio::inherit()).output().expect("cargo could not be started");
    assert!(output.status.success(), "building the echo example failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact" && message["target"]["name"] == "echo")
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .expect("cargo reported no executable for the echo example")
}

/// 新建一个临时的插件目录：可执行文件按清单的 `entry` 命名（Windows 上宿主补 `.exe`），连同清单原样放进去。
fn plugin_directory(executable: &Path) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("xihan-echo-rust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the plugin directory could not be created");
    std::fs::copy(executable, directory.join(format!("echo{}", std::env::consts::EXE_SUFFIX)))
        .unwrap_or_else(|error| panic!("{} could not be copied: {error}", executable.display()));
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join("echo").join("xihan.manifest.json");
    std::fs::copy(manifest, directory.join("xihan.manifest.json")).expect("the manifest could not be copied");
    directory
}
