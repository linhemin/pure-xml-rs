#!/usr/bin/env bash
# 用攻击样本 + W3C 子集作为初始语料（语料不入库）。在 crate 根目录运行。
set -euo pipefail
W3C="${PUREXML_W3C_DIR:-tests/w3c/cache/xmlconf}"
for t in parse reader dom_roundtrip; do
  mkdir -p "fuzz/corpus/$t"
  cp tests/data/attacks/*.xml "fuzz/corpus/$t/"
  if [[ -d "$W3C/xmltest" ]]; then
    for d in valid/sa not-wf/sa; do
      for f in "$W3C/xmltest/$d"/*.xml; do cp "$f" "fuzz/corpus/$t/w3c-${d//\//-}-$(basename "$f")"; done
    done
  fi
done
echo "语料：$(ls fuzz/corpus/parse | wc -l) 个文件/目标"
# dom_mutation：输入 = 2 字节种子长度 + 种子 XML + 操作流；种子取攻击样本与 W3C valid/sa，操作流给一段固定字节
mkdir -p fuzz/corpus/dom_mutation
for f in fuzz/corpus/dom_roundtrip/*; do
  n=$(stat -c %s "$f")
  if (( n < 4096 )); then
    { printf "$(printf '\\%03o\\%03o' $((n >> 8)) $((n & 255)))"; cat "$f"; printf '\000\006\001\013\003\005\017\007\002\020\011\021\012\015\016'; } > "fuzz/corpus/dom_mutation/$(basename "$f")"
  fi
done
echo "dom_mutation 语料：$(ls fuzz/corpus/dom_mutation | wc -l) 个文件"
