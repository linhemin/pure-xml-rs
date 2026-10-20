# fuzz 回归样本（tests/data/fuzz-regressions/）

`tests/fuzz_regressions.rs` 会把本目录下所有非 `.md` 文件当作输入，分别用 `from_bytes` 和
`from_reader`（BufReader 容量 1、7、64）解析，要求：不 panic、2 秒内结束、峰值内存 < 64 MiB、两种入口结果一致。

新的 crash / timeout / OOM 样本先用 `cargo +nightly fuzz tmin <目标> <样本>` 最小化，再按
`<目标>-<类别>-<哈希前 8 位>.bin` 命名放进来，并在下表登记。

| 文件 | 来源 | 说明 |
|---|---|---|
| parse-slow-asan-e9e98bb9.bin | M1 测试轮 parse 目标第 1 次运行（2026-10-20 22:59 台北时间） | libFuzzer 报 timeout（57 秒），但两个目标在同一秒同时“超时”，判断是宿主机暂停造成的假超时。非插桩 release 下 0.6 ms 结束（Encoding：malformed UTF-8）。留作冒烟样本 |
| reader-slow-asan-87bd6bea.bin | 同上，reader 目标 | 同上；release 下 1.1 ms 结束（SecurityLimit(ExpansionRatio)） |

M1 测试轮：第 3 轮 parse、reader 各连续跑 900 秒（另有前两轮约 25 分钟，期间只有 box 暂停导致的假超时），没有发现真实的 crash / OOM / 超时。
