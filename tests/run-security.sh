#!/usr/bin/env bash
# 用虚拟内存上限兜底跑安全套件：先编译，再在 ulimit -v 下只运行测试二进制。
# 用法（crate 根目录）：tests/run-security.sh [额外 cargo test 参数]
set -euo pipefail
LIMIT_KB="${PUREXML_VMEM_KB:-4194304}"   # 4 GiB 虚拟内存
cargo test --release --test security_tests --no-run "$@"
BIN=$(cargo test --release --test security_tests --no-run --message-format=json "$@" 2>/dev/null \
  | grep -o '"executable":"[^"]*security_tests[^"]*"' | tail -1 | cut -d'"' -f4)
echo "[run-security] $BIN（ulimit -v ${LIMIT_KB} KiB）"
( ulimit -v "$LIMIT_KB"; "$BIN" --test-threads=4 )
