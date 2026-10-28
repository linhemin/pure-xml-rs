# purexml

纯 Rust、安全优先的 XML 工具库。0.x 阶段只提供**流式拉取解析器**和**基础写出器**；DOM、XPath、校验与完整往返留到后续里程碑。

**0.x API 不稳定**，在 1.0 之前事件、配置字段和错误结构都可能调整。

解析器不包含任何 `unsafe`，也没有运行时依赖或 C 绑定。它**从不**根据 system id / public id 打开文件或网络；调用方传入的 `BufRead` 只是字节来源，解析器不会把外部标识符解释成路径。

## 用法

```rust
use purexml::{Event, Reader};

let xml = br#"<root id="1">hello</root>"#;
let mut reader = Reader::from_bytes(xml);

let start = reader.read_event().unwrap().unwrap();
match start {
    Event::StartElement(element) => {
        assert_eq!(element.name.local.as_ref(), "root");
        let id = element.attribute("id").unwrap();
        assert_eq!(id.value.as_ref(), "1");
        for attr in element.attributes() {
            assert_eq!(attr.name.local.as_ref(), "id");
        }
    }
    other => panic!("unexpected {other:?}"),
}

match reader.read_event().unwrap().unwrap() {
    Event::Text(text) => assert_eq!(text.as_ref(), "hello"),
    other => panic!("unexpected {other:?}"),
}
assert!(matches!(
    reader.read_event().unwrap().unwrap(),
    Event::EndElement(_)
));
assert!(reader.read_event().unwrap().is_none());
```

`read_event` 返回 `Result<Option<Event>>`。`Ok(None)` 表示根元素之后的输入已经结束。元素名的前缀和本地名、文本、CDATA、注释、处理指令、XML 声明和 `Doctype` 的字符串字段都是 `XmlText`：不需要换行规范化或实体展开时借用输入（`is_borrowed()`），否则是共享的 `Arc<str>`。下一次 `read_event` 之前必须放下当前事件；要留下副本就调用 `into_owned()`（或 `into_cow()`）。`Event`、`XmlText`、`QName`、`Attribute` 和 `Error` 都是 `Send + Sync`。

`QName::namespace` 是驻留后的 `Option<Arc<str>>`，同一绑定上的元素和带前缀属性共享这一份 URI。用 `as_deref()` 当成 `&str` 比较。`Namespace` 声明里的前缀和 URI 则是 `XmlText`。

未声明的一般实体是语法错误：

```rust
use purexml::{ErrorKind, Reader};

let mut reader = Reader::from_str("<a>&nope;</a>");
let err = loop {
    match reader.read_event() {
        Ok(Some(_)) => {}
        Ok(None) => panic!("expected an error"),
        Err(err) => break err,
    }
};
assert!(matches!(err.kind, ErrorKind::Syntax));
assert_eq!(err.position.line, 1);
```

## 写出

文本和属性的转义不同：属性额外转义双引号。

```rust
use purexml::Writer;

let mut writer = Writer::new(Vec::new());
writer.write_start("note", &[("lang", "a<b&\"c")]).unwrap();
writer.write_text("x<y&z>").unwrap();
writer.write_end("note").unwrap();
let xml = String::from_utf8(writer.into_inner()).unwrap();
assert_eq!(
    xml,
    "<note lang=\"a&lt;b&amp;&quot;c\">x&lt;y&amp;z&gt;</note>"
);
```

## 安全默认

这些限制都放在公开的 `ParserConfig` 里，默认值是同名常量。超出任一限制会得到 `ErrorKind::SecurityLimit`，并用 `LimitKind` 标出是哪一个。`limit` / `actual` 与该限制使用同一单位。

| 常量 | 默认值 | `LimitKind` |
| --- | --- | --- |
| `DEFAULT_MAX_DEPTH` | 256 | `ElementDepth` |
| `DEFAULT_MAX_ATTRS` | 1024 | `Attributes` |
| `DEFAULT_MAX_NAME_LEN` | 1024 字节 | `NameLength` |
| `DEFAULT_MAX_TEXT_LEN` | 16 MiB | `TextLength`（单个文本节点） |
| `DEFAULT_MAX_TEXT_LEN` | 16 MiB | `AttrValueLength`（规范化并展开之后的单个属性值；写入时逐字符检查，`actual` 是第一次超过上限时的长度，`limit` 就是 `max_text_len`） |
| `DEFAULT_MAX_TOTAL_INPUT` | 1 GiB | `TotalInput`（`actual` 恒为 `limit + 1`，位置在越界的那个字节） |
| `DEFAULT_MAX_TOKEN_LEN` | 32 MiB | `TokenLength`（一个顶层 token 的原始字节，从 token 的第一个字节到终止符，不含前瞻。序言/尾声空白和 UTF-8 BOM 不计入；它们仍计入 `TotalInput`，偏移按原始字节。`actual` 恒为 `limit + 1`，位置是越界的那个字节。切片和流式都检查） |
| `DEFAULT_MAX_INTERNAL_SUBSET_LEN` | 16 MiB | `InternalSubsetLength`（内部子集 `[` 与 `]` 之间的原始字节；`actual` 是第一次超过时的长度，位置是越界的那个字节） |
| `DEFAULT_MAX_ENTITY_DEPTH` | 8 | `EntityDepth`（文档中的引用算第 1 层，第 9 层失败） |
| `DEFAULT_MAX_ENTITY_EXPANSIONS` | 10_000 | `EntityExpansions`（同一个实体的重复引用也计数；10_000 次可以，10_001 次失败） |
| `DEFAULT_MAX_EXPANDED_BYTES` | 1 MiB | `ExpandedBytes` |
| `DEFAULT_MAX_EXPANSION_RATIO` | 10 | `ExpansionRatio` |
| `EXPANSION_RATIO_FLOOR_BYTES` | 64 KiB | 比率分子不超过该值时不检查比率 |

展开比率在**每一次**一般实体展开，以及**每一次**补上默认属性时计算：

- 分子：到目前为止，所有展开产生的替换文本字节数；每补上一个默认属性，都把它的名字字节和**完整值字节**加进分子。值虽然是一份共享的 `Arc<str>`，但和实体展开一样，每次都按完整长度计数。同一个大默认值用在很多元素上时，可能需要调高 `max_expansion_ratio`
- 这些默认属性字节**不**计入 1 MiB 的 `ExpandedBytes`，也**不**计入实体展开次数
- 分母：到目前为止已经消费的**原始输入**字节数（解析游标 `base + pos`，不是整个缓冲区的长度，也不把替换文本算进分母）。切块方式不改变这个游标
- 只有分子 **大于** 64 KiB 之后才比较
- 若分子大于 `分母 * max_expansion_ratio`，错误为 `ExpansionRatio`。此时 `limit` 是当时允许的字节数（`已消费字节 * 比率`，乘法饱和），`actual` 是比率分子。分子用饱和加法，加到 `u64::MAX` 就停住，不会 panic
- `max_expansion_ratio = 0`：分子一旦大于 64 KiB，允许量是 0，下一次展开或补默认属性就报 `ExpansionRatio`。不超过地板时仍然通过
- `max_expansion_ratio = u64::MAX`：比率检查关闭。一般实体展开仍受 1 MiB 的 `ExpandedBytes` 约束

内部一般实体默认可用，但受上述上限约束。预定义实体（`lt` `gt` `amp` `apos` `quot`）和字符引用不计入展开次数。不存在任何开关可以打开外部实体或外部 DTD 加载。

### DTD 与实体

- `<!DOCTYPE>` 上的 `SYSTEM` / `PUBLIC` **不是错误**。外部子集直接跳过、从不读取；`Doctype` 事件会带上 `system_id` 和 `public_id`。
- 内部子集里的一般实体声明会记录并允许在文档中展开。替换文本必须本身是良构的 `content`：标签、注释、CDATA、处理指令不能跨过实体边界。
- 带 `SYSTEM` 或 `PUBLIC` 的**实体声明**（包括外部参数实体）是 `ExternalEntityForbidden`。记法声明上的 SYSTEM/PUBLIC 只是标识符，会做语法检查，不会去加载。
- 内部参数实体声明（`<!ENTITY % name "...">`）允许，只记录、永不使用。
- 任何参数实体引用 `%name;` 都是 `ExternalEntityForbidden`。属性默认值引号里的 `%` 是普通字符，不是参数实体引用。
- 引用没有内部声明的一般实体是 `Syntax`（即使 DOCTYPE 指向了外部 DTD，因为那个 DTD 不会被加载）。`ATTLIST` 默认值里的一般实体也按这条检查，而且必须在使用前已经声明。
- `ELEMENT` / `ATTLIST` / `NOTATION` 会做语法检查。`ATTLIST` 的默认值会补到元素上；非 `CDATA` 类型在普通属性规范化之后再折叠空白。属性值里经实体展开引入的字面 `<` 是 `Syntax`。
- 属性值长度在接受每个字符时检查（纯文本快路径和需要规范化或实体展开的路径都一样）。超过 `max_text_len` 就停在那个字符，不再把后面的内容拷进缓冲。`actual` 等于当时的长度（上限加上该字符的 UTF-8 字节数）。刚好等于上限可以通过。
- 注释、CDATA 和 PI 内容也按字符检查 `max_text_len`，不是等到终止符。停在第一个超限字符上，`actual` 是计入该字符后的长度，位置是该字符之后的游标。未闭合的超长注释报 `TextLength`，不会先扫完整份注释。
- PI 目标、一般实体名、参数实体名、记法名（包括 `NDATA` 引用的记法名）必须是 NCName，不能含冒号。实体引用里的名字同样按 NCName 读，所以 `&a:b;` 是名字错误，不是“未定义实体”。`DOCTYPE` 名必须是 QName：没有冒号，或恰好一个冒号且两边都是 NCName（`a:b` 可以，`r:`、`:r`、`a:b:c` 不行）。Reader 和 Writer 用同一套规则。
- 字符引用只接受小写 `&#x`。位数不设上限，数值超出 XML `Char` 才拒绝。内容里的 `]]>` 只禁止三个字面字符连在一起；`]]&gt;` 是合法写法，解析结果是字符 `]]>`。
- 实体替换文本的良构检查用堆上的元素栈，嵌套超过 `max_depth` 是 `SecurityLimit(ElementDepth)`，不会把线程栈打爆。`ELEMENT` 内容模型也用堆上的显式栈，调用栈深度与嵌套层数无关；层数仍受 `max_depth` 限制，并且不超过 1024 层，超过是 `SecurityLimit(ElementDepth)`。
- `ATTLIST` 默认值在声明处做一次 UTF-8 校验（非 `CDATA` 再折叠空白），结果放进一个 `Arc<str>`。之后每个元素只克隆这个 `Arc`，不再扫描原文。属性去重按驻留 URI 的指针比较，不按 URI 内容哈希。每次补上默认属性，都把名字字节和完整值字节计入展开比率的分子；默认值用得很重时，可能需要调高 `max_expansion_ratio`。
- 属性值和其他事件字符串都是 `XmlText`：字面量借用输入（`is_borrowed()`），共享默认值用 `Arc`。用 `as_str()` / `Deref` 取文本，用 `into_owned()` 或 `into_cow()` 拿走所有权。

## 工具链

库的 MSRV 是 1.70，没有运行时依赖。`cargo test` 会编译 dev-dependencies：`quick-xml` 0.42 要求 Rust 1.86，测试代码里的 `Option::is_none_or` 要求 1.82。本仓库本地用 stable 1.99.1 跑测试、clippy 和 fmt。不要把 `quick-xml` 降到 0.39（有安全通告）。见 `CONTRIBUTING.md`。

## 增量读取

`Reader::from_reader` / `from_reader_with_config` 返回 `Result<Reader<IoInput<R>>>`（构造本身总是 `Ok`，真正的 I/O 和限额错误出现在 `read_event`）。`Reader::from_vec` 仍按整份缓冲区解析，规则与 `from_bytes` 相同。

流式读取在交给解析器之前先划定**一个完整的顶层 token**（外加 1 字节前瞻，或真到 EOF）。不会从 token 起点重跑解析器，因此实体展开计数不会因为补数据而算两遍。文本节点不按读块拆开，事件序列与 `from_bytes` 一致。

窗口大约是 `max_token_len` 再加一个 8 KiB 的读块，和文档总长无关。已经消费的前缀会压缩掉。`max_token_len` 小于 `max_text_len` 时，长文本会先碰到 `TokenLength`。只由 CR LF 组成的超长 token，规范化后的长度大约是原始长度的一半，默认 32 MiB 的 token 上限可能比 16 MiB 的文本上限更早触发。

`max_total_input` 按位置判定，切片和流式相同：只有解析真正需要 offset = limit 的那个字节时才报 `TotalInput`。位置是这个字节，`actual` 恒为 `limit + 1`。在它之前已经完整结束的 token 会先产出事件；位置更靠前的错误优先，不会因为缓冲区更长就提前拒绝。流式窗口仍然大约是 `max_token_len` 再加一个读块，不会为了这条限制把后续输入全部读进来。

I/O：`ErrorKind::Interrupted` 会一直重试。`WouldBlock` 和其他 I/O 错误变成 `ErrorKind::Io` 并熔断（之后每次 `read_event` 都返回同一个错误）。I/O 错误的 `offset` 是已经成功读入的原始字节数。`Ok(0)` 是 EOF，之后的字节不再读取。`read` 若声称读到的字节数大于缓冲区，报 `ErrorKind::Io`，不 panic。用户 reader 自己的 panic 会原样抛出。

任何错误（语法、限额、编码、I/O）之后，切片和流式都会熔断。

## 留到 M2

- DOM、XPath、校验和完整往返。

### 编码

只接受 UTF-8。UTF-8 BOM 会跳过。UTF-16 BOM、无 BOM 的 UTF-16 `<?` 模式，或声明中的 UTF-16 / 其他编码，都是 `ErrorKind::Encoding`。0.x **不做转码**。非法 UTF-8 同样是 `Encoding`；非法 XML 码点（包括字符引用）是 `Syntax`。

### 错误形状

```text
Error { kind, position, message }

position: Position { line, column, offset }  // 行、列从 1 开始，offset 为原始输入字节，从 0 开始

ErrorKind::SecurityLimit { kind: LimitKind, limit: u64, actual: u64 }
ErrorKind::ExternalEntityForbidden
ErrorKind::Syntax
ErrorKind::Encoding
ErrorKind::Io

LimitKind::EntityDepth | EntityExpansions | ExpandedBytes | ExpansionRatio
        | ElementDepth | Attributes | NameLength | TextLength
        | TotalInput | AttrValueLength | TokenLength | InternalSubsetLength
```

列按 Unicode 标量值计数。按字节计数的 `TokenLength`、`TotalInput`、`InternalSubsetLength`，位置是越界字节本身。按字符计数的 `TextLength`、`AttrValueLength`、`NameLength`，位置是越过上限的那个字符之后的游标。展开过程中的错误位置指向原文档里最外层 `&`。

## 模块

- `purexml`：门面，重新导出 core。
- `purexml-core`：`config`（限制与 `ParserConfig`）、`error`、`event`、`reader`（拉取解析）、`input`（`IoInput` 与定界）、`writer`（转义写出）、`rules`（NCName、QName、命名空间绑定，Reader 和 Writer 共用）。名字字符类和实体良构检查是私有实现细节。

## 许可证

MIT OR Apache-2.0。见 `LICENSE-MIT` 与 `LICENSE-APACHE`。
