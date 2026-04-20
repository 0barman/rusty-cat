#!/usr/bin/env bash
# 1) 清空 Cargo 构建缓存（target）及集成测试在系统临时目录留下的下载/上传文件
# 2) 扫描 src/ 与 tests/ 中是否存在 unsafe；若存在则报错并退出
# 3) 一次性运行 crates 下 tests/ 目录的全部集成测试（cargo test --tests）
#
# 用法：在任意目录执行
#   bash /path/to/rusty-cat/scripts/full-verify.sh
# 或
#   cd rusty-cat && ./scripts/full-verify.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${ROOT}"

echo "==> [1/3] 清空 Cargo target 与测试临时文件…"
cargo clean

# 集成测试多在 std::env::temp_dir() 下写入 rusty_cat_* 或固定名 task-not-found*.bin
clean_temp_dir() {
  local d="$1"
  [[ -d "$d" ]] || return 0
  find "$d" -maxdepth 1 \( -name 'rusty_cat_*' -o -name 'task-not-found.bin' -o -name 'task-not-found-*.bin' \) \
    -type f -print -delete 2>/dev/null || true
}

if [[ -n "${TMPDIR:-}" ]]; then
  clean_temp_dir "${TMPDIR}"
fi
clean_temp_dir "/tmp"

echo "==> [2/3] 检查 src/ 与 tests/ 中是否包含 unsafe…"
scan_unsafe() {
  if command -v rg >/dev/null 2>&1; then
    rg -l --glob '*.rs' '\bunsafe\b' "${ROOT}/src" "${ROOT}/tests" >/dev/null 2>&1
  else
    grep -rEl --include='*.rs' '\bunsafe\b' "${ROOT}/src" "${ROOT}/tests" >/dev/null 2>&1
  fi
}

if scan_unsafe; then
  echo "错误: 在 rusty-cat 源码或测试中发现 unsafe，已中止。" >&2
  echo "---- 匹配位置 ----" >&2
  if command -v rg >/dev/null 2>&1; then
    rg -n --glob '*.rs' '\bunsafe\b' "${ROOT}/src" "${ROOT}/tests" >&2 || true
  else
    grep -rEn --include='*.rs' '\bunsafe\b' "${ROOT}/src" "${ROOT}/tests" >&2 || true
  fi
  exit 1
fi

echo "==> [3/3] 运行 tests/ 下全部集成测试（cargo test --tests）…"
cargo test --tests

echo "==> 全部完成。"
