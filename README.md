# FastDL — 多线程分段高速下载器

Rust 实现的 TDM Fast 风格分段并行下载引擎 + Tauri 2 macOS GUI。

## 仓库结构

```
fastdl/         下载引擎（crate + CLI）
  - HTTP 能力探测（Range 支持 / 总大小，含 206 无 Content-Range 的回退探测）
  - 多线程 Range 分段并行下载（TDM Fast 同款调度：段队列 + 工作池）
  - 断点续传（块边界存 .fastdl.state.json，续传从已完成字节继续）
  - 暂停/取消（cancel flag，块边界生效，状态保留）
  - 总大小未知时自动降级单流下载（不确定进度）
  - 完成后可选 SHA-256 校验
fastdl-app/     Tauri 2 macOS GUI 壳（FastDL.app）
  - 任务队列 + 实时进度条/速度 + 暂停/继续/重试/移除 + Finder 定位 + 目录选择
  - 前端无打包器（原生 window.__TAURI__），命令经 IPC 直连引擎
  - GUI 命令管线由无头 mock_app 集成测试覆盖（tests/gui_pipeline.rs）
```

## 构建

前置：Rust 工具链 + tauri-cli（`cargo install tauri-cli --version "^2"`）。

```sh
# 引擎 CLI
cd fastdl && cargo build --release
./target/release/fastdl <url> -o <out> -s 32 -t 8 [-r] [-p] [--verify <sha256>]

# GUI（macOS）
cd fastdl-app/src-tauri && cargo tauri build
# 产物: target/release/bundle/macos/FastDL.app
```

## 说明

- GUI 未签名，本机运行无碍；分发需 codesign + notarize。
- 引擎测试：`cd fastdl && cargo test --release`（含全链路 e2e）。
- GUI 管线测试：`cd fastdl-app/src-tauri && cargo test --test gui_pipeline`。
