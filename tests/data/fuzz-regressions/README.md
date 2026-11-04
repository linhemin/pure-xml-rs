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
| writer_roundtrip-crash-63fb89f9.bin | M1 R4 确认性回归 / r8 writer_roundtrip（原始样本，未最小化） | D23：DOCTYPE 名 `rxmlns:` 带 PUBLIC 标识 |
| writer_roundtrip-crash-f2ef4724.bin | 同上（原始样本） | D23：DOCTYPE 名 `docxml:` 带内部子集 |
| writer_roundtrip-crash-12a2f21b.bin | M1 R9 续跑（r9b）writer_roundtrip（原始样本，7 KB） | D23：PI 目标 `xmlns:` |
| writer_roundtrip-crash-22c64e90.bin | 同上（原始样本） | D23：DOCTYPE 名 `r:` 带内部子集 |
| writer_roundtrip-crash-5c3a3134.bin | 同上（原始样本） | D23：PI 目标 `:ml` |
| writer_roundtrip-crash-a7187392.bin | 同上（原始样本） | D23：DOCTYPE 名 `rYPE333:` |
| writer_roundtrip-crash-fd3c5623.bin | 同上（原始样本） | D23：内容里的 PI 目标 `:p` |
| reader_1byte-crash-9318d3f4.bin | M2 PR #3 r2c 轮 reader_1byte（6894e02 起即存在；原始样本 `fuzz-artifacts/r2c-6894e02/reader_1byte/crash-9318d3f4…`，122 字节，未最小化） | 真实缺陷 D40：内部子集 `<!NOTAt`（关键字前缀后失配），限额字节在失配字节之前时 from_bytes 报 Syntax、流式报 TokenLength。harness 语义（首字节选紧限额，`[1,1,1]` 块长）的断言见 `incremental_m2.rs::d40_fuzz_samples_replay_like_reader_1byte`（期望 TokenLength 48/49 @61）；最小复现 `d40_fuzz_minimal_repros`（min-9318，@48）。本文件的默认配置重放只要求两种入口一致 |
| reader_1byte-crash-f0f243d8.bin | 同上（原始样本 `crash-f0f243d8…`，233 字节） | D40：内部子集 `<!NOTATt`；最小复现 min-f0f2（TokenLength 48/49 @48） |

M1 测试轮：第 3 轮 parse、reader 各连续跑 900 秒（另有前两轮约 25 分钟，期间只有 box 暂停导致的假超时），没有发现真实的 crash / OOM / 超时。

### D23：已在 M2 PR-A（72924c7）修复

r8、r9、r9b 的 D23 样本去重后共 9 个：2 个已最小化（`-min.xml`），7 个保留原始字节（`.bin`，在修复前的代码上才会崩溃，修复后无法再用 tmin 最小化；各自的最小模式写在 `regression_m2.rs::d23_fuzz_samples_rejected` 里）。修复后这 9 个样本全部被 Reader 以 Syntax 拒绝，`tests/regression_m2.rs::d23_fuzz_samples_rejected` 逐个断言（from_bytes 和逐字节 from_reader 一致）。

**从 PR-A 起，writer_roundtrip 不再有已知模式，任何崩溃都按 NEW 分诊。** `d23_colons_in_pi_entity_notation_names_rejected` 已去掉 `#[ignore]`，断言不变。

<details><summary>修复前的已知模式（历史记录）</summary>


`fuzz/fuzz_targets/writer_roundtrip.rs` **不对 D23 做任何过滤或跳过**。在 D23 修复（M2）之前，writer_roundtrip 预计还会反复报出下面这类“解析成功、写不回去”的崩溃，分拣时归入 D23，不算新缺陷：

- PI 目标含冒号：`<?a: x?>`、`<?a:b x?>`（Writer 报 `Syntax("empty name")` 或 invalid name）
- DOCTYPE 名不是 QName：`<!DOCTYPE r: ...>`、`<!DOCTYPE a:b:c>`
- 实体名、记号名含冒号（只出现在内部子集里，Reader 不产生对应事件，一般不会触发往返失败）

崩溃信息里 panic 的事件是 `ProcessingInstruction` 或 `Doctype`、名字含 `:`，就属于这一类。其他任何 writer_roundtrip 崩溃仍按新缺陷分拣。断言在 `tests/regression_m1.rs::d23_colons_in_pi_entity_notation_names_rejected`（`#[ignore = "M2: D23"]`，用 `cargo test -- --ignored` 运行）。

</details>
