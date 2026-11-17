//! 从规范的 `spec/plugin/plugin.proto` 重新生成插件运行时的协议代码 `src/plugin/generated/xihan.plugin.v1.rs`。
//!
//! 在仓库根执行 `cargo run --manifest-path tools/protogen/Cargo.toml`；改了规范子模块的指针后运行一次，把生成物一并提交。
//! `.proto` 由纯 Rust 的 protox 编译，不需要 protoc。

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let root: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", ".."].iter().collect();
    let descriptors = protox::compile(["plugin.proto"], [root.join("spec").join("plugin")])?;
    let out = root.join("src").join("plugin").join("generated");
    std::fs::create_dir_all(&out)?;
    tonic_prost_build::configure()
        // 插件实现 Plugin 服务、回调 Host 服务；两边的服务端与客户端都生成，测试里扮演宿主时要用
        .build_server(true)
        .build_client(true)
        // 连接由运行时自己建立（Unix 域套接字、命名管道、回环 TCP），不生成按地址连接的便捷入口
        .build_transport(false)
        .emit_rerun_if_changed(false)
        .out_dir(&out)
        .compile_fds(descriptors)?;
    println!("generated {}", out.join("xihan.plugin.v1.rs").display());
    Ok(())
}
