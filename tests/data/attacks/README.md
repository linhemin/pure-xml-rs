# 攻击样本库（tests/data/attacks/）

所有期望结果均指 **`ParserConfig::default()`**。错误类别：
`SecurityLimit`（限额，需带行列号与限额种类）/ `ExternalEntityForbidden`（下文简写 **EEF**）/ `Syntax` / `Encoding`；`Ok` 表示必须成功解析（边界用例）。

通用要求（每个样本都适用）：不 panic、2 秒内返回、峰值内存有界（见 `tests/security_tests.rs`）、零文件/网络 IO。

- 入库样本：本目录下的 `*.xml`，均 < 100 KB，由 `make_samples.py` 生成（可重建）。
- 大样本：`python3 make_samples.py --large` 生成到 `generated/`（已 `.gitignore`）。`cargo test` **不依赖** Python，测试在内存里生成同样的数据。
- 网络样本统一写成 `127.0.0.1:18080`，测试时会替换成随机监听端口，并断言 0 连接。
- 文件样本里的 `/etc/passwd` 在测试中替换成临时“金丝雀”文件，断言其内容没有出现在任何事件里。

## M1 默认限额（Lead 已定）
| 限额 | 默认值 |
|---|---|
| 实体嵌套深度 | 8（8 层通过，9 层报错） |
| 实体展开次数 | 10_000 |
| 展开后总字节（绝对上限） | 1 MiB |
| 展开比例 | 展开后总字节 ≤ 输入字节 × 10；展开总量 ≤ 64 KiB 时不检查比例 |

所有限额都可以通过 ParserConfig 调整，超出报 SecurityLimit（带行列号和限额种类）。

计数口径（Lead 已定）：内容里的顶层引用算第 1 层，深度上限 8 = 最多 8 层嵌套；展开次数按每次展开计，重复引用同一实体每次都算。
比例口径（Lead / Implementer 已定）：**流式检查**——展开总量（替换文本字节累加）超过 64 KiB 之后，每次展开都检查 展开总量 ≤ 已读输入字节 × 10。
**M1 接入后的口径（Implementer 已定，2026-10-20 测试轮写入断言）**：
- 递归（直接或间接）→ Syntax；非法码点（字符引用 / 原文）→ Syntax。
- 预定义实体（`&lt;` 等 5 个）和字符引用不算“展开”，不计入次数、深度、展开字节（S6o）。
- 注释、CDATA、PI 的长度受 TextLength 约束（同一 16 MiB 上限）。
- `xmlns` / `xmlns:p` 声明计入 max_attrs（1024）。
- 所有限额的“超过”都是严格大于：恰好等于上限通过。
- ExpansionRatio：分母 = 顶层输入游标（引用 `;` 之后的偏移）；报错时 `limit` = 已读 × 10，`actual` = 累计替换文本字节（S6k/S6m/S6n 精确断言这两个数）。

样本按“已读输入 = 本次引用的 `;` 之后的偏移”调参：填充注释放在引用之前，引用之后只剩 `</r>\n`（5 字节），比例随每次引用单调上升，最后一次展开最紧。若实现把“已读”定义在引用开头（`&` 处）或按缓冲块计，S6l/S6m 需要按 `make_samples.py` 里的 `ratio_doc(total_len=…)` 微调。

属性值超长：报 SecurityLimit(**AttrValueLength**)，上限取 max_text_len（16 MiB）。
| 元素深度 | 256（根元素算第 1 层；256 通过，257 报错） |
| 每个元素的属性数 | 1024 |
| 名称长度 | 1024 字节（按字节计） |
| 单个文本节点 | 16 MiB |
| 总输入 | 1 GiB（测试中调低上限验证，不真生成） |
| 外部实体 / 外部 DTD | 永不加载，无开关 |
| `<!DOCTYPE r SYSTEM/PUBLIC ...>` 外部 DTD | 不报错，跳过外部子集继续解析；Doctype 事件如实带 SYSTEM/PUBLIC 标识 |
| 引用只在外部 DTD 里定义的实体 | Syntax（未定义实体） |
| 带 SYSTEM/PUBLIC 的实体声明（含外部参数实体） | EEF（声明时就报） |
| 内部参数实体 | 只声明不引用：Ok；引用 `%x;`：EEF |
| UTF-16 BOM / UTF-16 编码声明 | Encoding（M1 不支持） |

## 样本清单

### S1 XXE（file://）
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S1a | S1a-xxe-file.xml | 外部通用实体 `SYSTEM "file:///etc/passwd"` 并引用 | EEF |
| S1b | S1b-xxe-file-relative.xml | 相对路径 SYSTEM | EEF |
| S1c | S1c-xxe-public.xml | PUBLIC 外部实体 | EEF |
| S1d | S1d-xxe-declared-unused.xml | 只声明不引用 | EEF（声明即拒绝） |
| S1e | S1e-xxe-in-attribute.xml | 属性值中引用外部实体 | EEF |
| S1f | S1f-xxe-unparsed-ndata.xml | NDATA 未解析实体 | EEF |

### S2 XXE（http://，SSRF）
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S2a | S2a-xxe-http.xml | `SYSTEM "http://127.0.0.1:18080/xxe"` | EEF，0 连接 |
| S2b | S2b-xxe-https.xml | https 变体 | EEF，0 连接 |

### S3 外部参数实体 / 外部 DTD
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S3a | S3a-external-pe.xml | 外部参数实体声明 + `%ext;` | EEF（声明时就报），0 连接 |
| S3h | S3h-external-pe-declared-unused.xml | 外部参数实体只声明不引用 | EEF（声明时就报），0 连接 |
| S3b | S3b-external-dtd-http.xml | `<!DOCTYPE r SYSTEM "http://…">` | **Ok**，0 连接；Doctype 的 system_id 如实给出 |
| S3c | S3c-external-dtd-file.xml | `<!DOCTYPE r SYSTEM "file:///etc/passwd">`（测试中指向金丝雀文件） | **Ok**，文件内容不得被读入 |
| S3d | S3d-external-dtd-public.xml | `<!DOCTYPE r PUBLIC "-//EVIL//DTD" "http://…">` | **Ok**，0 连接；public_id 和 system_id 如实给出 |
| S3e | S3e-external-dtd-plus-subset.xml | 外部 DTD + 内部子集定义 `&ok;` | **Ok**，0 连接；内部实体正常展开为 "fine" |
| S3g | S3g-entity-only-in-external-dtd.xml | 引用只在外部 DTD 中定义的 `&fromext;` | Syntax（未定义实体），0 连接 |
| S3f | S3f-oob-exfil.xml | 经典 OOB 外带（外部 PE + PE 嵌套构造外部实体） | EEF，0 连接 |

### S4 Billion Laughs
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S4a | S4a-billion-laughs.xml | 经典 lol9（10 层 × 10） | SecurityLimit |
| S4b | S4b-laughs-shallow-wide.xml | 只 4 层（不碰深度上限）× 20，靠次数/字节触发 | SecurityLimit |
| S4c | S4c-laughs-in-attribute.xml | 在属性值中展开 lol9 | SecurityLimit |

### S5 二次膨胀
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S5a | S5a-quadratic-small.xml | 10 KiB 实体 × 2000 次（≈20 MB） | SecurityLimit |
| S5b | generated/S5b-quadratic-50k-x-50k.xml | 50 KiB × 5 万次（≈2.4 GB），生成 | SecurityLimit |
| S5c | generated/S5c-abs-1mib-exact-ok.xml | 展开总量恰好 1 MiB（64 KiB × 16），输入恰好 110000 字节（10 倍 > 1 MiB，不触发比例） | **Ok** |
| S5d | generated/S5d-abs-1mib-plus1.xml | 展开总量 1 MiB + 1，输入相同 | SecurityLimit(ExpandedBytes) |

### S6 递归 / 嵌套深度 / 展开次数 / 展开比例
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S6a | S6a-recursive-ab.xml | a→b→a | Syntax（规范 WFC: No Recursion；Implementer 已定：递归一律报 Syntax，不报 SecurityLimit） |
| S6b | S6b-self-ref.xml | 自引用 | 同上 |
| S6c | S6c-recursive-in-attr.xml | 属性值中递归 | 同上 |
| S6d | S6d-entity-depth-8-ok.xml | e1→…→e8 共 8 层 | **Ok** |
| S6e | S6e-entity-depth-9.xml | 9 层 | SecurityLimit |
| S6f | S6f-expansions-10000-ok.xml | 平铺引用 10_000 次 | **Ok** |
| S6g | S6g-expansions-10001.xml | 平铺引用 10_001 次 | SecurityLimit |
| S6h | S6h-unbalanced-markup-in-entity.xml | 实体替换文本含不平衡标签 | Syntax |
| S6i | S6i-undeclared-entity.xml | 引用未声明实体 | Syntax |
| S6j | S6j-ratio-exact-64k-ok.xml | 展开总量恰好 65536（1 KiB × 64），输入 1294 字节（≈50 倍） | **Ok**（≤64 KiB 不查比例） |
| S6k | S6k-ratio-64k-plus1.xml | 展开总量 65537（再加 1 次 1 字节实体），≈50 倍 | SecurityLimit(ExpansionRatio) |
| S6l | S6l-ratio-exact-10x-ok.xml | 展开总量 204800（2 KiB × 100）；最后一次展开时已读恰好 20480 字节（文件 20485 字节）→ 恰好 10 倍 | **Ok** |
| S6m | S6m-ratio-10x-plus.xml | 同样的展开量；最后一次展开时已读 20479 字节（文件 20484 字节）→ 略超 10 倍 | SecurityLimit(ExpansionRatio) |
| S6o | S6o-predefined-charrefs-not-counted-ok.xml | `&lt;` × 10001 + `&#60;` × 10001（90017 字节）：预定义实体和字符引用不算展开次数 | Ok |
| S6n | S6n-ratio-streaming-only.xml | 前部 4 KiB × 100 次展开（409600 字节；最后一次展开时已读 4453 字节，≈92 倍），后部 60000 字节普通文本；全文 64458 字节，整体只有 ≈6.35 倍。只有流式检查能拦下；“已读”多算到 36 KB（预读/截止点差异）仍 >10 倍；不碰 1 MiB / 1 万次 | SecurityLimit(ExpansionRatio) |

S6d/S6e（8/9 层）与 S6f/S6g（10_000/10_001 次）均已压线：嵌套链 e1→…→eN 正文引用 &e1; 算第 1 层；平铺引用单个 1 字节实体，展开总量远低于 64 KiB，不会误触比例或 1 MiB 上限。

### S7 元素深度
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S7a | S7a-depth-256-ok.xml | 256 层（根元素算第 1 层） | **Ok** |
| S7b | S7b-depth-257.xml | 257 层 | SecurityLimit |
| S7c | generated/S7c-depth-100000.xml | 10 万层，生成 | SecurityLimit，不栈溢出 |
| S7d | generated/S7d-depth-100000-unclosed.xml | 10 万层只开不闭 | SecurityLimit |

### S8 名称 / 属性
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S8a | S8a-duplicate-attr.xml | 重复属性 | Syntax |
| S8b | S8b-duplicate-attr-ns.xml | 不同前缀绑定同一 URI 的同名属性 | Syntax（命名空间约束） |
| S8c | S8c-unbound-prefix.xml | 未绑定前缀 | Syntax |
| S8d | S8d-xmlns-rebind.xml | 绑定保留前缀 `xmlns` | Syntax |
| S8e | S8e-lt-in-attr.xml | 属性值含 `<` | Syntax |
| S8k | S8k-attrs-1024-ok.xml | 1024 个属性 | **Ok** |
| S8l | S8l-attrs-1025.xml | 1025 个属性 | SecurityLimit(Attributes) |
| S8m | S8m-elem-name-1024-ok.xml | 元素名 1024 字节 | **Ok** |
| S8n | S8n-elem-name-1025.xml | 元素名 1025 字节 | SecurityLimit(NameLength) |
| S8o | S8o-attr-name-1024-ok.xml | 属性名 1024 字节 | **Ok** |
| S8p | S8p-attr-name-1025.xml | 属性名 1025 字节 | SecurityLimit(NameLength) |
| S8q | S8q-name-1024-bytes-multibyte-ok.xml | 512 个 `é` = 1024 字节 | **Ok** |
| S8r | S8r-name-1025-bytes-multibyte.xml | 512 个 `é` + `a` = 1025 字节（验证按字节而非字符计） | SecurityLimit(NameLength) |
| S8s | S8s-end-name-1025.xml | 结束标签名 1025 字节 | SecurityLimit 或 Syntax（不得绕过、不得崩溃） |
| S8f | generated/S8f-long-element-name-1mib.xml | 1 MiB 元素名 | SecurityLimit(NameLength) |
| S8g | generated/S8g-long-attr-name-1mib.xml | 1 MiB 属性名 | SecurityLimit(NameLength) |
| S8h | generated/S8h-long-attr-value-16mib-plus1.xml | 属性值 16 MiB + 1 | SecurityLimit(AttrValueLength) |
| S8y | （仅测试中） | 属性值恰好 16 MiB | **Ok** |
| S8i | generated/S8i-many-attrs-100k.xml | 10 万个属性 | SecurityLimit(Attributes) |
| S8j | generated/S8j-long-text-64mib.xml | 64 MiB 文本 | SecurityLimit(TextLength) |
| S8t | generated/S8t-text-16mib-ok.xml | 文本恰好 16 MiB | **Ok** |
| S8u | generated/S8u-text-16mib-plus1.xml | 文本 16 MiB + 1 | SecurityLimit(TextLength) |
| S8v/S8w | （仅测试中） | max_total_input 调低到 4096：4096 字节通过、4097 字节报 SecurityLimit(TotalInput)；1 GiB 不真生成 | Ok / SecurityLimit |
| S8x | （仅测试中） | 调高 max_attributes / max_name_len 后 S8l、S8n 通过；max_text_len 调到 1024 后：文本、属性值 1024 通过，1025 分别报 TextLength / AttrValueLength；max_attributes 调到 2 后 3 个属性报错 | 可调整 |

### S9 参数实体（只声明不引用 OK；引用 `%x;` → EEF）
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S9a | S9a-pe-builds-external.xml | PE 拼出外部实体声明 | EEF |
| S9b | S9b-pe-in-markup-decl.xml | PE 出现在标记声明内部（内部子集中规范也禁止） | EEF（允许 Syntax） |
| S9c | S9c-pe-internal-simple.xml | 纯内部 PE 引用 `%p;` | EEF |
| S9d | S9d-pe-laughs.xml | 2000 次 PE 引用 | EEF |
| S9e | S9e-pe-declared-unused-ok.xml | 内部 PE 只声明不引用 | **Ok** |

### S10 编码
| ID | 文件 | 说明 | 期望 |
|---|---|---|---|
| S10a | S10a-utf8-bad-continuation.xml | `C3 28` | Encoding |
| S10b | S10b-utf8-lone-continuation.xml | 孤立 `80` | Encoding |
| S10c | S10c-utf8-overlong-2byte.xml | 过长编码 `C0 AF` | Encoding |
| S10d | S10d-utf8-overlong-3byte.xml | `E0 80 AF` | Encoding |
| S10e | S10e-utf8-overlong-4byte.xml | `F0 80 80 AF` | Encoding |
| S10f | S10f-utf8-surrogate-high.xml | 代理区 `ED A0 80`（U+D800） | Encoding |
| S10g | S10g-utf8-surrogate-low.xml | `ED BF BF`（U+DFFF） | Encoding |
| S10h | S10h-utf8-above-10ffff.xml | `F4 90 80 80` | Encoding |
| S10i | S10i-utf8-invalid-ff-fe.xml | `FF FE` 字节 | Encoding |
| S10j | S10j-utf8-truncated-eof.xml | 文件末尾截断的多字节序列 | Encoding |
| S10k | S10k-utf8-bad-in-name.xml | 元素名中非法字节 | Encoding |
| S10l | S10l-utf8-bad-in-attr.xml | 属性值中代理区 | Encoding |
| S10m | S10m-utf8-bad-in-comment.xml | 注释中 `C0 80` | Encoding |
| S10n | S10n-utf8-bad-in-cdata.xml | CDATA 中 5 字节序列 | Encoding |
| S10o | S10o-utf8-5byte-seq.xml | `F8 88 80 80 80` | Encoding |
| S10p | S10p-utf16le-bom.xml | UTF-16LE BOM | Encoding（M1 不支持 UTF-16） |
| S10q | S10q-utf16be-bom.xml | UTF-16BE BOM | Encoding |
| S10r | S10r-utf16-truncated.xml | UTF-16 奇数字节截断 | Encoding |
| S10s | S10s-utf16-declared-no-bom.xml | 声明 UTF-16 但无 BOM | Encoding |
| S10t | S10t-utf8-bom-declares-utf16.xml | UTF-8 BOM + 声明 UTF-16（冲突） | Encoding |
| S10u | S10u-utf16le-bom-declares-utf8.xml | UTF-16LE BOM + 声明 UTF-8（冲突） | Encoding |
| S10v | S10v-utf32le-bom.xml | UTF-32LE BOM | Encoding |
| S10w | S10w-unsupported-encoding.xml | 声明未知编码 | Encoding |
| S10x | S10x-xml-illegal-char-ctrl.xml | 合法 UTF-8 但 XML 非法字符 U+0001 | Syntax |
| S10y | S10y-xml-illegal-nul.xml | NUL 字节 | Syntax |
| S10z | S10z-xml-illegal-fffe.xml | U+FFFE | Syntax |
| S10za | S10za-charref-surrogate.xml | `&#xD800;` | Syntax |
| S10zb | S10zb-charref-zero.xml | `&#0;` | Syntax |
| S10zc | S10zc-charref-too-big.xml | `&#x110000;` | Syntax |

### S11 无外部实体开关
不是样本文件：`tests/security_tests.rs::s11_no_external_entity_switch` 断言 `ParserConfig` 不存在任何开启外部实体/外部 DTD/resolver 的字段或方法（编译期 + 文档/源码扫描两层）。
