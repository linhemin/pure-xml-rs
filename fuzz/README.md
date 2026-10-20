# 模糊测试（cargo-fuzz，本地运行，零费用）

前置：`rustup toolchain install nightly`、`cargo install cargo-fuzz`（本机已装，见 STATUS-test.md）。

```bash
# 在 crate 根目录
fuzz/seed-corpus.sh
cargo +nightly fuzz run parse  -- -max_total_time=900 -timeout=2 -rss_limit_mb=1024 -dict=fuzz/xml.dict
cargo +nightly fuzz run reader -- -max_total_time=900 -timeout=2 -rss_limit_mb=1024 -dict=fuzz/xml.dict
```

M1 目标：两个目标累计 ≥ 30 分钟，无 crash / 超时 / OOM。
发现的崩溃：`fuzz/artifacts/<target>/crash-*` → 最小化（`cargo +nightly fuzz tmin`）→ 拷到
`tests/data/fuzz-regressions/` 并加回归测试 → 反馈 Implementer。

两个目标已接入 M1 真实 API（`Reader::from_bytes` / `Reader::from_reader`）。

注：开启 ASAN 后即使占位目标 RSS 也有约 300–380 MB，所以 `-rss_limit_mb` 取 1024，而不是 512。
本机冒烟（2026-10-20，占位目标）：`parse` 跑了 20 秒、`reader` 跑了 10 秒，构建和运行都正常。
