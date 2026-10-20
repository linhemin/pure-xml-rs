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

`read_event` 返回 `Result<Option<Event>>`。`Ok(None)` 表示根元素之后的输入已经结束。事件里的字符串在不需要换行规范化或实体展开时借用输入（`Cow::Borrowed`），否则为 `Cow::Owned`。下一次 `read_event` 之前必须放下当前事件。

`QName::namespace` 是 `Option<Rc<str>>`，同一绑定上的元素和带前缀属性共享这一份 URI。用 `as_deref()` 当成 `&str` 比较。

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
| `DEFAULT_MAX_TEXT_LEN` | 16 MiB | `AttrValueLength`（规范化并展开之后的单个属性值，`limit` 就是 `max_text_len`） |
| `DEFAULT_MAX_TOTAL_INPUT` | 1 GiB | `TotalInput` |
| `DEFAULT_MAX_ENTITY_DEPTH` | 8 | `EntityDepth`（文档中的引用算第 1 层，第 9 层失败） |
| `DEFAULT_MAX_ENTITY_EXPANSIONS` | 10_000 | `EntityExpansions`（同一个实体的重复引用也计数；10_000 次可以，10_001 次失败） |
| `DEFAULT_MAX_EXPANDED_BYTES` | 1 MiB | `ExpandedBytes` |
| `DEFAULT_MAX_EXPANSION_RATIO` | 10 | `ExpansionRatio` |
| `EXPANSION_RATIO_FLOOR_BYTES` | 64 KiB | 累计替换文本不超过该值时不检查比率 |

展开比率按流式方式在**每一次**一般实体展开时计算：

- 分子：到目前为止，所有展开产生的替换文本字节数（每次展开加上该实体的替换文本长度）
- 分母：到目前为止已经消费的**原始输入**字节数（解析游标，不是整个缓冲区的长度，也不把替换文本算进分母）
- 只有分子 **大于** 64 KiB 之后才比较
- 若分子大于 `分母 * max_expansion_ratio`，错误为 `ExpansionRatio`。此时 `limit` 是当时允许的替换字节数（`已消费字节 * 比率`），`actual` 是累计替换字节数

内部一般实体默认可用，但受上述上限约束。预定义实体（`lt` `gt` `amp` `apos` `quot`）和字符引用不计入展开次数。不存在任何开关可以打开外部实体或外部 DTD 加载。

### DTD 与实体

- `<!DOCTYPE>` 上的 `SYSTEM` / `PUBLIC` **不是错误**。外部子集直接跳过、从不读取；`Doctype` 事件会带上 `system_id` 和 `public_id`。
- 内部子集里的一般实体声明会记录并允许在文档中展开。替换文本必须本身是良构的 `content`：标签、注释、CDATA、处理指令不能跨过实体边界。
- 带 `SYSTEM` 或 `PUBLIC` 的**实体声明**（包括外部参数实体）是 `ExternalEntityForbidden`。记法声明上的 SYSTEM/PUBLIC 只是标识符，会做语法检查，不会去加载。
- 内部参数实体声明（`<!ENTITY % name "...">`）允许，只记录、永不使用。
- 任何参数实体引用 `%name;` 都是 `ExternalEntityForbidden`。属性默认值引号里的 `%` 是普通字符，不是参数实体引用。
- 引用没有内部声明的一般实体是 `Syntax`（即使 DOCTYPE 指向了外部 DTD，因为那个 DTD 不会被加载）。`ATTLIST` 默认值里的一般实体也按这条检查，而且必须在使用前已经声明。
- `ELEMENT` / `ATTLIST` / `NOTATION` 会做语法检查。`ATTLIST` 的默认值会补到元素上；非 `CDATA` 类型在普通属性规范化之后再折叠空白。属性值里经实体展开引入的字面 `<` 是 `Syntax`。
- 字符引用只接受小写 `&#x`。位数不设上限，数值超出 XML `Char` 才拒绝。内容里的 `]]>` 只禁止三个字面字符连在一起；`]]&gt;` 是合法写法，解析结果是字符 `]]>`。
- 实体替换文本的良构检查用堆上的元素栈，嵌套超过 `max_depth` 是 `SecurityLimit(ElementDepth)`，不会把线程栈打爆。`ELEMENT` 内容模型的括号嵌套同样受 `max_depth` 限制，并且不超过 1024 层。
- `ATTLIST` 默认值若是输入里的一段原文，事件直接借用这段原文；需要规范化的默认值只分配一次，用 `Rc` 共享。属性去重按驻留 URI 的指针比较，不按 URI 内容哈希。
- 属性值的类型是 `XmlText`：字面量借用输入（`is_borrowed()`），共享默认值用 `Rc`。用 `as_str()` / `Deref` 取文本。

## 工具链

库的 MSRV 是 1.70，没有运行时依赖。`cargo test` 会编译 dev-dependencies：`quick-xml` 0.42 要求 Rust 1.86，测试代码里的 `Option::is_none_or` 要求 1.82。本仓库本地用 stable 1.99.1 跑测试、clippy 和 fmt。不要把 `quick-xml` 降到 0.39（有安全通告）。见 `CONTRIBUTING.md`。

## 留到 M2

- **D11**：属性值只要走过实体展开或空白规范化，长度要等整个值读完才检查 `AttrValueLength`。临时缓冲仍受输入大小和 1 MiB 展开上限约束，安全套件里的 16 MiB+1 会报错。
- DOM、XPath、校验和完整往返。`from_reader` 仍会把输入缓冲到 `max_total_input`。

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
        | TotalInput | AttrValueLength
```

列按 Unicode 标量值计数。展开过程中的错误位置指向原文档里最外层 `&`。

## 模块

- `purexml`：门面，重新导出 core。
- `purexml-core`：`config`（限制与 `ParserConfig`）、`error`、`event`、`reader`（拉取解析）、`writer`（转义写出）。名字字符类和实体良构检查是私有实现细节。

## 许可证

MIT OR Apache-2.0。见 `LICENSE-MIT` 与 `LICENSE-APACHE`。
