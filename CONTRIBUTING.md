# 参与开发

## Rust 版本

库本身的 MSRV 是 **1.70**（`rust-version`）。`purexml` 与 `purexml-core` 不使用更高版本才有的语言特性，也没有运行时依赖。

测试、benchmark 和与 `quick-xml` 0.42 的对照可以需要更新的 stable。`quick-xml` 0.42 要求 Rust **1.86**。请保持 0.42，不要退回 0.39（该版本有安全通告）。在 1.86 之前的工具链上不要把 `quick-xml` 加进默认 `cargo test` 依赖。

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

W3C XML 一致性套件不入库。需要时：

```bash
tests/w3c/fetch.sh
cargo test --test w3c_suite -- --nocapture
```

`tests/w3c/known-failures.txt` 记录当前仍失败的用例。缓存目录 `tests/w3c/xmlconf/` 和 zip 已忽略。
