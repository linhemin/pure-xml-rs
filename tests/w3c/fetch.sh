#!/usr/bin/env bash
# 下载 W3C XML Conformance Test Suite (xmlts20130923) 到本地缓存。
# 免费、官方地址；大文件不入库（见 tests/w3c/.gitignore）。
#
# 用法：
#   tests/w3c/fetch.sh                # 默认缓存到 tests/w3c/cache/
#   W3C_CACHE=/path/to/dir tests/w3c/fetch.sh
#
# 完成后可设置环境变量供运行器使用：
#   export PUREXML_W3C_DIR=<缓存目录>/xmlconf
set -euo pipefail

URL="https://www.w3.org/XML/Test/xmlts20130923.tar.gz"
# 已知文件大小（字节），2013-09-23 发布，至今未变
EXPECTED_SIZE=641522
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="${W3C_CACHE:-$SCRIPT_DIR/cache}"
TARBALL="$CACHE/xmlts20130923.tar.gz"

mkdir -p "$CACHE"

if [[ -f "$TARBALL" ]] && [[ "$(stat -c %s "$TARBALL")" == "$EXPECTED_SIZE" ]]; then
  echo "[fetch] 已存在缓存：$TARBALL"
else
  echo "[fetch] 下载 $URL"
  curl -fSL --retry 3 --connect-timeout 20 -o "$TARBALL.part" "$URL"
  mv "$TARBALL.part" "$TARBALL"
fi

SIZE="$(stat -c %s "$TARBALL")"
if [[ "$SIZE" != "$EXPECTED_SIZE" ]]; then
  echo "[fetch] 警告：大小 $SIZE 与预期 $EXPECTED_SIZE 不符" >&2
fi
echo "[fetch] sha256: $(sha256sum "$TARBALL" | cut -d' ' -f1)"

echo "[fetch] 校验压缩包完整性"
tar -tzf "$TARBALL" > /dev/null

rm -rf "$CACHE/xmlconf"
tar -xzf "$TARBALL" -C "$CACHE"

XT="$CACHE/xmlconf/xmltest"
[[ -f "$XT/xmltest.xml" ]] || { echo "[fetch] 缺少 $XT/xmltest.xml" >&2; exit 1; }

count() { find "$XT/$1" -maxdepth 1 -name '*.xml' -type f | wc -l; }
VALID_SA=$(count valid/sa)
NOTWF_SA=$(count not-wf/sa)
# 清单中的条目数（TYPE 统计）
MANIFEST_VALID_SA=$(grep -c 'URI="valid/sa/' "$XT/xmltest.xml" || true)
MANIFEST_NOTWF_SA=$(grep -c 'URI="not-wf/sa/' "$XT/xmltest.xml" || true)

echo "[fetch] xmltest/valid/sa   文件数：$VALID_SA（清单条目：$MANIFEST_VALID_SA）"
echo "[fetch] xmltest/not-wf/sa  文件数：$NOTWF_SA（清单条目：$MANIFEST_NOTWF_SA）"
echo "[fetch] 完成。export PUREXML_W3C_DIR=$CACHE/xmlconf"
