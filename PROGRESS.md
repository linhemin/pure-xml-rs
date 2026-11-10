# 进度与交接

只打开 README 时，读「交接（必读）」即可接着做。本文件是同一快照的依据和会话记录。

每次会话在本文件**顶部**「会话记录」下新增一节，并同步 README 里的「当前状态」。两处都写：

- 日期（会话当天）
- 这次改了什么（提交、文件、行为）
- 还剩什么
- 阻塞（失败的测试、未决决定、没做完的原因）；没有就写「无」
- 相关 PR 或 issue 链接；没有就写「无」

只写仓库、测试和 PR 里对得上的事实。不新编里程碑、日期、PR 号或已经完成的工作。额度、上下文或用量预算快用完时，停掉功能修改，先把 README 的「当前状态」和本节写完。只提交代码、没有交接的会话是未完成的。

开工前读完 README「交接」和本文件，再看代码。多个 AI 各自开工，看不到彼此的对话。

## 会话记录

### 2026-11-10 — 写交接规则和现状快照

- 改了什么：在 `README.md` 增加「交接（必读）」（简体中文，含强制规则和当前状态）。新增本文件。没有改解析器、写出器、DOM、测试、用法示例、安全默认、编码、模块列表或许可证。
- 还剩什么：见下方「下一步」。库功能与快照时的 `main` 相同。
- 阻塞：无。
- 链接：本变更的 PR 合并后，把链接补进下一节。调查时用到的 PR 见「已合并的 PR」。
- 本轮没有运行 `cargo test`、W3C 或 bench。下面的测试数字若来自 PR 说明，会标明日期，不当成 2026-11-10 的新结果。

## 下一步

1. 读草稿 [PR #6](https://github.com/linhemin/pure-xml-rs/pull/6) 的分支 `cursor/purexml-m2c2-mutate`（调查时头为 `7c4b750`）。不要在 `main` 上平行再写一套 DOM 修改或序列化。
2. PR #6 还缺公开的文档类型构建入口。`create_doctype` 是 `pub(crate)`。`tests/c2_m2.rs` 的 `make_doctype` 固定返回 `None`，所以下面两个集成测试仍是 `#[ignore]`：
   - `doctype_public_without_system_rejected`
   - `doctype_omit_option_drops_doctype`
   忽略说明写的是「等 M2 收尾的公开构建入口」。分支上 `edit.rs` 的 `public_id_without_system_id_leaves_the_document_unchanged` 已用 crate 内构造拒绝「只有 PUBLIC、没有 SYSTEM」（`DomError::Hierarchy`）。`ser.rs` 的 `omit_doctype_drops_a_system_identifier` 覆盖省略文档类型。`doctype_public_without_system_rejected` 的忽略说明仍写「ser.rs 暂无对应单元测试」，这句比 `9d9bddc` 旧。
   `da01826` 把 `create_doctype` 留在 crate 内，提交说明写明：公开构造可以写出 `SYSTEM` 标识符，对下游解析器是 XXE 风险。要改可见性，先把这个风险和序列化规则一起定下来。
3. PR #6 合入 `main` 之后，这些能力仍然没有，也没有对应的未合并 PR，仓库里没有排好的先后，不要新编里程碑：
   - 从 `Reader` 建 DOM。`crates/purexml-core/src/dom/document.rs` 写着 “Parsing does not build nodes yet.” PR #6 分支上这句还在。
   - XPath。仓库里没有 XPath 模块。
   - 校验。
   - README 文首说的完整往返。`crates/purexml-core/src/writer.rs` 写明 0.x 的 `Writer` 不是完整序列化器，DOM 往返不在该写出器的 0.x 范围内。PR #6 的序列化是另一条路径：DOM 经 `Writer::write_event` 写出。
4. 用户向文档与 `main` 不一致。见「文档和代码不一致」。那是单独的文档工作。不要顺手改用法、安全默认表、编码、模块和许可证，除非这一轮的任务就是同步它们。

## 快照范围

- 日期：2026-11-10
- `main`：`4a5cafbb1f93a26d3d7bdf63bcd26ab9a76dd671`（`4a5cafb`）
- 提交说明：`Merge pull request #5 from linhemin/cursor/purexml-m2c1-dom`，2026-11-04，标题 “M2-C1: DOM arena/构建/限额/遍历”
- 版本：根 `Cargo.toml` 与 `purexml-core` 都是 `0.1.0`。没有 git tag。
- MSRV：`1.99.1`。edition：`2024`。工作区成员：`.` 与 `crates/purexml-core`。`fuzz/` 是独立 workspace（`purexml-fuzz`，`publish = false`）。
- 许可证：MIT OR Apache-2.0。
- 2026-11-10 用 `gh issue list` / `gh pr list` 查看：没有 issue（打开的和全部都没有）。打开的 PR 只有 #6，且是草稿。

## 已合并的 PR

| PR | 状态 | 标题 |
| --- | --- | --- |
| [#1](https://github.com/linhemin/pure-xml-rs/pull/1) | 已合并 | M1：安全优先的流式 XML 解析器与基础写出器 |
| [#2](https://github.com/linhemin/pure-xml-rs/pull/2) | 已合并 | M2 PR-A：属性值增量限长与共享名字规则（D11 / D23） |
| [#3](https://github.com/linhemin/pure-xml-rs/pull/3) | 已合并 | Stream documents one token at a time |
| [#4](https://github.com/linhemin/pure-xml-rs/pull/4) | 已合并 | Security: count `>` in text toward max_text_len (D37) |
| [#5](https://github.com/linhemin/pure-xml-rs/pull/5) | 已合并，2026-11-04 | M2-C1: DOM arena/构建/限额/遍历 |

PR #5 说明里记录的是合并当时的验证，不是 2026-11-10 重跑的结果：debug `cargo test --workspace` 375 passed / 9 ignored，release 379 passed / 9 ignored，`--ignored` 9 passed。W3C 计数与当前 `tests/w3c/known-failures.txt` 的分类数量一致（见下表）。fuzz 六个目标各跑了约 121 秒，退出码 0。说明里还有一组 `PXB_SAMPLES=10`、对照 `18050d4` 的 bench，没有一组慢过 10%。这些数字都写在 PR 说明里，原始 bench 输出不在本仓库。

## main 上已有的行为

与 README 用户文档一致、并且代码对得上的部分：

- `Reader::read_event` 拉取解析。`from_reader` 先划定一个完整顶层 token，再解析。见 `crates/purexml-core/src/reader.rs`、`input.rs`。
- 安全默认在 `ParserConfig` 和 README「安全默认」表里。外部实体、外部 DTD 没有打开的开关。
- 只接受 UTF-8。UTF-16 BOM 或声明中的其他编码是 `ErrorKind::Encoding`。
- `Writer` 能写开始/空元素/结束、文本、CDATA、注释、处理指令和 `write_event`。文本与属性的转义不同。
- `CHANGELOG.md` 的 `[Unreleased]` 另外记下、README 安全一节也描述了的行为：MSRV 从 1.70 升到 1.99.1；`TotalInput` 按位置报告（D31）；实体引用名是 NCName；注释、CDATA、PI 按字符检查 `max_text_len`（D26）；文本中的 `>` 计入 `max_text_len`。

M2-C1 的只读 DOM（PR #5，在 `main` 上）：

- 模块：`crates/purexml-core/src/dom/mod.rs`、`document.rs`。门面 `purexml` 经 `purexml_core::*` 重新导出。
- `Document::new` / `with_config` 得到只有文档节点的空文档。`NodeId` 带 document id 和 generation。槽位不复用。删除后是 tombstone，`Document::get` 对它返回 `None`。
- 读 API：父子与兄弟遍历、元素名、属性、命名空间查找、文本、PI、文档类型的名字和外部标识。文档类型不保存内部子集。
- 限额：`DEFAULT_MAX_NODES` = 10_000_000，`DEFAULT_MAX_DOM_BYTES` = 1 GiB。属性不算节点。字节数是逻辑记录，不是分配器容量。
- `dom/mod.rs` 写明：这一步不解析、不提供公开修改 API、不序列化。建树辅助函数在 `#[cfg(test)]` 里（`add_element`、`add_text` 等）。
- README 的安全默认表没有这两条 DOM 限额。

## 草稿 PR #6（未合并）

- 链接：<https://github.com/linhemin/pure-xml-rs/pull/6>
- 分支：`cursor/purexml-m2c2-mutate`
- 2026-11-10 看到的头：`7c4b750`（“Check for a root element when writing the document node.”）
- 状态：草稿。说明正文里的 head 仍是 `c0f49a0`。其后的提交是 `9d9bddc`、`7c4b750`。以分支为准，不要只信说明正文。
- 说明正文记录的验证同样是当时的，不是本轮重跑：debug `cargo test --workspace` 437 passed / 17 ignored，release 441 passed / 17 ignored；`--ignored` 为 6 passed、2 failed。说明里点名仍失败的就是上面两个文档类型集成测试。fuzz 在说明里多了 `dom_roundtrip` 和 `dom_mutation`。
- `main` 没有、该分支有的文件：`crates/purexml-core/src/dom/edit.rs`、`ser.rs`，`tests/c2_impl.rs`、`tests/c2_m2.rs`，`fuzz/fuzz_targets/dom_mutation.rs`、`dom_roundtrip.rs`。
- 分支上的公开修改与写出（`create_doctype` 除外）：`create_element`、`create_text`、`create_cdata`、`create_comment`、`create_pi`、`append_child`、`insert_before`、`insert_after`、`detach`、`remove`、`set_attribute`、`remove_attribute`、`declare_namespace`、`remove_namespace`、`set_text`、`set_pi_data`、`cmp_document_order`；`Document::write_to` / `write_to_with` / `to_string` / `to_string_with`；`NodeRef::write_to` / `write_to_with`。
- 说明里已经写明、提交也对得上的行为：无前缀属性不在默认命名空间里；CDATA、注释、PI 里的 CR 在创建或设置时是 `DomError::InvalidChar`，文档不变；没有根元素时 `Document::write_to` 和 `to_string` 返回 `WriteError::NoRootElement`；`7c4b750` 让文档节点上的 `NodeRef::write_to` 使用同一检查。

## 测试、fuzz、bench

根集成测试：`api_traits_m1`、`defects_m1`、`differential`、`extra_m1`、`extra_m2`、`fuzz_regressions`、`guard_selftest`、`incremental_m2`、`regression_d37`、`regression_m1`、`regression_m2`、`security_tests`、`stack_depth_dtd_m1`、`stack_depth_m1`、`stack_depth_r3_m1`、`w3c_runner`、`writer_tests`。core 还有 `crates/purexml-core/tests/m1.rs`。

`main` 上的 `#[ignore]` 都在 `tests/incremental_m2.rs`，原因写的是 slow（全切块或大组合），不是失败占位。PR #5 说明写 `--ignored` 9 passed。个数与当前 `main` 上这 9 处 ignore 一致，通过与否以当时的说明为准。

W3C：`tests/w3c/fetch.sh` 下载到 `tests/w3c/cache/`（gitignore）。`tests/w3c/known-failures.txt` 当前 16 条，全部带策略类别：

| ID | 类别 |
| --- | --- |
| not-wf-sa-140 | policy-5e |
| not-wf-sa-141 | policy-5e |
| valid-sa-012 | policy-ns |
| valid-sa-049 | policy-utf16 |
| valid-sa-050 | policy-utf16 |
| valid-sa-051 | policy-utf16 |
| valid-sa-070 | policy-eef |
| valid-sa-082 | policy-eef |
| valid-sa-083 | policy-eef |
| valid-sa-091 | policy-eef |
| valid-sa-097 | policy-eef |
| valid-sa-100 | policy-eef |
| valid-not-sa-011 | policy-eef |
| valid-not-sa-012 | policy-eef |
| valid-not-sa-026 | policy-eef |
| valid-not-sa-031 | policy-ext-dtd-entity |

不在这份清单里的新失败，运行器会当成回归。不要为了清掉 `policy-eef` 或 `policy-utf16` 去加载外部实体或做转码；那和 README、`tests/w3c_runner.rs` 里写明的 M1 策略相反。

攻击样本在 `tests/data/attacks/`，期望写在该目录的 README。fuzz 回归样本在 `tests/data/fuzz-regressions/`。其中 D23 的说明写明已在 M2 PR-A（`72924c7`）修复。D40 的样本仍在目录里；`tests/incremental_m2.rs` 用用例锁关键字截断和按字节限额（含 `d40_fuzz_samples_replay_like_reader_1byte`）。默认配置重放只要求 `from_bytes` 与 `from_reader` 一致。

`main` 上的 fuzz 目标（`fuzz/Cargo.toml`）：`parse`、`reader`、`writer_roundtrip`、`parse_small_stack`、`reader_chunked`、`reader_1byte`。需要 nightly 和 cargo-fuzz。`fuzz/README.md` 仍写「两个目标」和「见 STATUS-test.md」；仓库里没有 `STATUS-test.md`。

Bench：`benches/baseline.rs`，记录在 `benches/BASELINE.md`。文件里的原始输出路径在 `/workspace/purexml/`，不在本仓库。最后一条记录是 `2e818ce`（2026-11-04）：from_bytes 相对 `257c900` 为 −2.5% 到 −9.6%（text-dense 1KB −8.1%）；from_vec −8.0% 到 +0.2%；from_reader 相对 `4d7ebe2` 为 config +3.2%、attr +3.9%、text +8.0%、ns-dense 1MB +4.5%。文中把 100 MB 档和「安全限制开销 ≤ 5%」留到它称为 M4 的工作，并写明 M1 不把性能当门槛。M2 的同时段 A/B 规则也只记录在该文件里。本轮没有新的 bench。

本地验收命令在 `CONTRIBUTING.md`：`cargo +1.99.1 test --workspace --no-fail-fast`，以及同工具链的 clippy `-D warnings` 和 `fmt --all --check`。dev-dependency 里的 `quick-xml` 保持 0.42，不要退回 0.39。

## 文档和代码不一致

这些是 2026-11-10 的差。用户向的用法、安全默认、编码、模块、许可证这一轮故意没改。

- README 文首和「留到 M2」把 DOM、XPath、校验和完整往返整项留到后面。`main` 上已经有只读 DOM arena。修改和序列化在未合并的 PR #6。解析建树、XPath、校验、完整往返仍然没有。
- README「模块」没有列出公开模块 `dom`。`DEFAULT_MAX_NODES` / `DEFAULT_MAX_DOM_BYTES` 从 `config` 重新导出；`limit` 模块本身是私有的。
- README 安全默认表没有 DOM 的节点数和字节上限。
- `CHANGELOG.md` 的 `[Unreleased]` 没有 PR #3 的增量读取，也没有 PR #5 的 DOM。
- `fuzz/README.md` 仍是两个目标，并指向不存在的 `STATUS-test.md`。
- `tests/defects_m1.rs` 指向不存在的 `M1-test-report.md`。
- `deny.toml` 的头注释写「M1 测试轮草案」。

## 本轮没有改写的用户文档

`README.md` 里从「用法」起的示例、安全默认、增量读取、「留到 M2」、编码、错误形状、模块和许可证保持原样。交接一节插在文首三段之后、「用法」之前。
