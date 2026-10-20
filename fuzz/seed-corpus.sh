#!/usr/bin/env bash
# 用攻击样本 + W3C 子集作为初始语料（语料不入库）。在 crate 根目录运行。
set -euo pipefail
W3C="${PUREXML_W3C_DIR:-tests/w3c/cache/xmlconf}"
for t in parse reader; do
  mkdir -p "fuzz/corpus/$t"
  cp tests/data/attacks/*.xml "fuzz/corpus/$t/"
  if [[ -d "$W3C/xmltest" ]]; then
    for d in valid/sa not-wf/sa; do
      for f in "$W3C/xmltest/$d"/*.xml; do cp "$f" "fuzz/corpus/$t/w3c-${d//\//-}-$(basename "$f")"; done
    done
  fi
done
echo "语料：$(ls fuzz/corpus/parse | wc -l) 个文件/目标"
