# fuzz 回归样本（tests/data/fuzz-regressions/）

`tests/fuzz_regressions.rs` 会把本目录下所有非 `.md` 文件当作输入，分别用 `from_bytes` 和
`from_reader`（BufReader 容量 1、7、64）解析，要求：不 panic、2 秒内结束、峰值内存 < 64 MiB、两种入口结果一致。

新的 crash / timeout / OOM 样本先用 `cargo +nightly fuzz tmin <目标> <样本>` 最小化，再按
`<目标>-<类别>-<哈希前 8 位>.bin` 命名放进来，并在下表登记。

| 文件 | 来源 | 说明 |
|---|---|---|
| parse-slow-asan-e9e98bb9.bin | M1 测试轮 parse 目标第 1 次运行（2026-10-20 22:59 台北时间） | libFuzzer 报 timeout（57 秒），但两个目标在同一秒同时“超时”，判断是宿主机暂停造成的假超时。非插桩 release 下 0.6 ms 结束（Encoding：malformed UTF-8）。留作冒烟样本 |
| reader-slow-asan-87bd6bea.bin | 同上，reader 目标 | 同上；release 下 1.1 ms 结束（SecurityLimit(ExpansionRatio)） |
| parse-crash-stackoverflow-5830f432.bin | M1 第二轮 parse 目标（2026-10-21 00:21 台北时间），41 772 字节 | 真实缺陷 D18：实体替换文本内约 7000 层 `<a>` 嵌套，ASan 下栈溢出。非插桩 release 需要 8 MiB 栈下约 4.4 万层才溢出，最小化后的确定性复现见 `tests/stack_depth_m1.rs`（在内存中生成 10 万层，2 MiB 栈） |
| writer_roundtrip-crash-42afd53c-min.xml | M1 第二轮 writer_roundtrip 目标（2026-10-21 00:15 台北时间），由 188 字节手工最小化到 45 字节 | 真实缺陷 D17：ATTLIST 默认属性名 `a:` 没有经过 QName 校验就被补上，Writer 写回时报 `invalid name`。断言在 `tests/regression_m1.rs::d17_*`；这里只回放 Reader（不 panic、两种入口一致） |
| writer_roundtrip-crash-543336e4-min.xml | M1 第三轮 writer_roundtrip 目标（2026-10-21 01:02 台北时间，6b29230）最小化 | 真实缺陷 D23：Reader 接受 PI 目标 `a:`（Namespaces §7 禁止冒号），Writer 拒绝写出，往返失败。断言见 `regression_m1.rs::d23_*` |
| writer_roundtrip-crash-a91aac12-min.xml | M1 第三轮 writer_roundtrip 目标（2026-10-21 01:19 台北时间，ddaaacc）最小化 | 真实缺陷 D23（同族）：Reader 接受 DOCTYPE 名 `r:`（不是 QName），Writer 拒绝写出 |

M1 测试轮：第 3 轮 parse、reader 各连续跑 900 秒（另有前两轮约 25 分钟，期间只有 box 暂停导致的假超时），没有发现真实的 crash / OOM / 超时。

### D23 已知模式（Lead 第三轮裁决：D23 移到 M2，fuzz 不跳过）

`fuzz/fuzz_targets/writer_roundtrip.rs` **不对 D23 做任何过滤或跳过**。在 D23 修复（M2）之前，writer_roundtrip 预计还会反复报出下面这类“解析成功、写不回去”的崩溃，分拣时归入 D23，不算新缺陷：

- PI 目标含冒号：`<?a: x?>`、`<?a:b x?>`（Writer 报 `Syntax("empty name")` 或 invalid name）
- DOCTYPE 名不是 QName：`<!DOCTYPE r: ...>`、`<!DOCTYPE a:b:c>`
- 实体名、记号名含冒号（只出现在内部子集里，Reader 不产生对应事件，一般不会触发往返失败）

崩溃信息里 panic 的事件是 `ProcessingInstruction` 或 `Doctype`、名字含 `:`，就属于这一类。其他任何 writer_roundtrip 崩溃仍按新缺陷分拣。断言在 `tests/regression_m1.rs::d23_colons_in_pi_entity_notation_names_rejected`（`#[ignore = "M2: D23"]`，用 `cargo test -- --ignored` 运行）。
