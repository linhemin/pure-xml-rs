# 参与开发

## Rust 版本

库本身的 MSRV 是 **1.99.1**（`rust-version`，跟随本地 stable），edition **2024**。`purexml` 与 `purexml-core` 没有运行时依赖。

`cargo test` 会编译 dev-dependencies。请保持 `quick-xml` 0.42，不要退回 0.39（该版本有安全通告）。本地验收使用 stable **1.99.1**：

```bash
cargo +1.99.1 test --workspace --no-fail-fast
cargo +1.99.1 clippy --workspace --all-targets -- -D warnings
cargo +1.99.1 fmt --all --check
```

W3C XML 一致性套件不入库。缓存目录是 `tests/w3c/cache/`（`tests/w3c/.gitignore`）。

```bash
tests/w3c/fetch.sh
cargo +1.99.1 test --test w3c_runner -- --nocapture
PUREXML_W3C_UPDATE=1 cargo +1.99.1 test --test w3c_runner
```

`tests/w3c/known-failures.txt` 按用例 ID 记录当前仍失败的条目。不在这份清单里的新失败会被运行器当成回归。
