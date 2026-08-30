#!/usr/bin/env bash
# 1) 清空 Cargo 构建缓存（target）及集成测试在系统临时目录留下的下载/上传文件
# 2) 由 Rust 编译器检查 lib 与 tests，并禁止真实的 unsafe 代码
# 3) 一次性运行 tests/ 目录的全部集成测试（cargo test --tests）
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

echo "==> [2/3] 编译检查 lib 与 tests，并禁止 unsafe Rust 代码…"
cargo check --lib --tests --all-features --locked

echo "==> [3/3] 运行 tests/ 下全部集成测试（cargo test --tests）…"
cargo test --tests

echo "==> 全部完成。"
