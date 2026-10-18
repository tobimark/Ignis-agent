#!/usr/bin/env bash
# ============================================================================
# harness/acceptance.sh — Ignis 手动量化验收（真实模型；不进 CI）
#
# demo.md §七定了三条内部验收门槛：
#   1. 三个演示场景各连续成功 3 次（`demos 3`）
#   2. 20 次常见任务端到端成功率 ≥80%（`tasks 20`，清单在 harness/tasks.md）
#   3. 10 次服务重启不出现任务重复执行或记录丢失（`restarts 10`）
# 本脚本是这三条的执行器：连真实 OpenAI 兼容端点、跑真二进制、核对真数据库行与真 JSONL 日志。
#
# v0.2 **沿用这三条门槛**（定义、判定与退出码口径一字不变，没有新增门槛、没有放宽成功率）；
# v0.2 的两条新指标不进本脚本，落在自动化用例上（离线、hermetic，随 `cargo test` 跑）：
#   - 会话启停 10 次、恢复后有效历史逐条相等 → `apps/cli/tests/sessions.rs`
#     （`session_restore_keeps_the_canonical_history_across_ten_cycles`）；
#   - 流式 100 次（随机分片不丢不重） → `apps/cli/src/render/markdown.rs` 与
#     `packages/runtime/src/think.rs` 的属性测试（各 100 轮随机切分）。
# 验收的唯一事实来源是 `docs/acceptance.md` 的「v0.2 验收」段：本脚本的真机结果由它记录。
#
# 设计约束：
#   - 手动、可选、不进 CI（CI 不依赖网络与模型）；
#   - 只经环境变量取凭据；凭据缺一即拒绝启动（退出码 2），绝不把「跳过」伪装成「通过」；
#   - 全程使用临时 IGNIS_HOME 与临时工作区，绝不碰真实的 ~/.ignis；
#   - 退出时（含中断）停掉自己拉起的守护进程并清理临时目录；结果文件不含 key。
#
# 用法：
#   harness/acceptance.sh demos [N]      三个演示各连续 N 次（默认 3）
#   harness/acceptance.sh tasks [N]      harness/tasks.md 的前 N 条（默认 20），门槛 ≥80%
#   harness/acceptance.sh restarts [N]   启停守护进程 N 次（默认 10），查重复/丢失/残留
#   harness/acceptance.sh --dry-run demos [N]   只打印将要执行的命令，不联网、不写结果文件
#   harness/acceptance.sh --keep ...     保留临时目录（排查用）
#
# 必需环境变量（demos/tasks/restarts 通用）：
#   IGNIS_BASE_URL   任意 OpenAI 兼容端点，例如 https://your-endpoint/v1
#   IGNIS_MODEL      模型名
#   IGNIS_API_KEY    密钥（只读环境变量；不写入仓库、结果文件或日志）
# 可选环境变量：
#   IGNIS_BIN                     ignis 二进制（默认 target/release/ignis → target/debug → PATH）
#   IGNIS_ACCEPT_RUN_TIMEOUT      单次 run 的墙钟上限秒数（默认 420）
#   IGNIS_ACCEPT_C_WAIT           Demo C 等一轮调度运行的秒数（默认 180）
#   IGNIS_ACCEPT_RESTART_SETTLE   每次重启后的观察秒数（默认 6）
#   IGNIS_ACCEPT_RESTART_BASELINE_WAIT  restarts 等基线运行的秒数（默认 180）
#   IGNIS_ACCEPT_ALLOW_CI         在 CI 环境里显式放行（默认 CI 下拒绝运行；这是手动工具）
#
# 退出码：0 = 全部 PASS 且没有任何门槛 SKIP｜1 = 有 FAIL（含门槛未达）或门槛 SKIP（验收不完整）
#         2 = 前置检查未通过（缺环境/缺二进制/doctor 不通过）——不是「通过」
# 「跳过」永不伪装成「通过」：逐项 SKIP 带原因；门槛级 SKIP（如无 uv 的 Demo B）让整体结论
# 变成「验收不完整」并退出码 1——某条门槛从未被测量，就不能叫验收通过。
# ============================================================================
set -euo pipefail

# ── 路径与固定输入 ────────────────────────────────────────────────────────────
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
readonly EXAMPLES_DIR="$REPO_ROOT/examples"
readonly TASKS_FILE="$SCRIPT_DIR/tasks.md"
SECONDS=0
readonly START_TS="$(date +%Y%m%d-%H%M%S)"
readonly RESULTS_PATH="$SCRIPT_DIR/results-$START_TS.md"

# 与 README / e2e 用例逐字一致的 prompt
readonly PROMPT_A="分析这个项目的测试失败原因，修复它并重新运行测试"
readonly PROMPT_B_REMEMBER="这个项目使用 uv 管理 Python 环境，请记住"
readonly PROMPT_B_REUSE="帮我检查这个项目的依赖并运行测试"
readonly PROMPT_B_CORRECT="不对，这个项目改用 pip 了"
readonly PROMPT_C="创建一个调度任务：每隔 1 分钟运行 examples/test-server/check-health.sh --simulate 92，磁盘超过 85% 就用 notify_user 告警"
# restarts 模式刻意用 20s 间隔：重启周期必须真的产出新运行行（20s ≤ RESCAN 的 30s 窗口，能按时触发）；
# ≥1m 的「从未运行」job 即使修复后也要等到 created_at+间隔（60s）才首次到期，与重启周期不匹配。
# 「>30s 首次到期」路径由 gateway 修复的判别测试与 demos 的 Demo C（1 分钟档）覆盖，不在这里等。
readonly PROMPT_RESTART="创建一个调度任务：每隔 20 秒运行 examples/test-server/check-health.sh --simulate 92，磁盘超过 85% 就用 notify_user 告警"

# ── 运行参数（parse_args 覆盖） ───────────────────────────────────────────────
MODE="demos"
N=""
DRY_RUN=0
KEEP=0

RUN_TIMEOUT="${IGNIS_ACCEPT_RUN_TIMEOUT:-420}"
C_WAIT="${IGNIS_ACCEPT_C_WAIT:-180}"
RESTART_SETTLE="${IGNIS_ACCEPT_RESTART_SETTLE:-6}"
RESTART_BASELINE_WAIT="${IGNIS_ACCEPT_RESTART_BASELINE_WAIT:-180}"
readonly TASKS_GATE_PCT=80
# 与 gateway 的 `ignis_gateway::RESCAN_SECS` 对齐：Demo C 的 note 判定用——间隔 ≤ 这个窗口时，
# 「>30s 的首次到期」路径不会被本次运行覆盖（见 demo_c_once 的 expr note）。
readonly RESCAN_SECS=30

# ── 运行期状态 ────────────────────────────────────────────────────────────────
RUN_ROOT=""
ITEMS_FILE=""
GATES_FILE=""
TRACK_FILE=""
IGNIS_BIN=""
DB_BACKEND=""
BASE_URL=""
MODEL=""
API_KEY=""
TIMEOUT_OK=0
DOCTOR_RC=""
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
SKIP_GATE_COUNT=0
SKIP_GATE_NAMES=""
DRY_COUNT=0
LAST_TASK_PASS=1
JSONL_BYTES=0
# 由 check_task_log 追加、由各 demo/task 函数在本次判定结束时读取（bash 动态作用域）
FAILURES=""
# 三个演示的逐次计数（demos 模式的门槛用）
A_PASS=0
A_FAIL=0
B_PASS=0
B_FAIL=0
C_PASS=0
C_FAIL=0
# restarts 模式的累计计数（重复/孤儿跨轮取最大值：门槛行不能被后来的 0 覆盖掉先前非零）
R_DUP_FIRES=0
R_DUP_TASKS=0
R_ORPHANS=0
R_DUP_ERR=0
R_CYCLES_PASS=0
R_CYCLES_FAIL=0

log() { printf '[acceptance] %s\n' "$*"; }

usage() {
  cat <<'EOF'
用法：harness/acceptance.sh [demos|tasks|restarts] [N] [--dry-run] [--keep]

  demos [N]      三个演示场景各连续 N 次真实运行（默认 3）
  tasks [N]      harness/tasks.md 的前 N 条（默认 20），成功率门槛 80%
  restarts [N]   启停守护进程 N 次（默认 10），查 job_runs/tasks 重复、丢失与守护进程残留

  --dry-run      只做结构检查并打印将要执行的命令；不联网、不写结果文件
  --keep         保留临时 IGNIS_HOME 与工作区（排查用）
  -h, --help     显示本帮助

必需环境变量：IGNIS_BASE_URL、IGNIS_MODEL、IGNIS_API_KEY（缺一则退出码 2）。
结果文件：harness/results-<时间戳>.md（含逐项与门槛表；不含任何密钥）。
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -h|--help) usage; exit 0 ;;
      --dry-run) DRY_RUN=1 ;;
      --keep) KEEP=1 ;;
      demos|tasks|restarts) MODE="$1" ;;
      ''|*[!0-9]*) printf 'error: 未知参数：%s\n' "$1" >&2; usage >&2; exit 2 ;;
      *)
        if [ -n "$N" ]; then
          printf 'error: N 只能给一次（已有 %s，又收到 %s）\n' "$N" "$1" >&2
          exit 2
        fi
        N="$1"
        ;;
    esac
    shift
  done
  case "$MODE" in
    demos) N="${N:-3}" ;;
    tasks) N="${N:-20}" ;;
    restarts) N="${N:-10}" ;;
  esac
  case "$N" in
    ''|*[!0-9]*|0) printf 'error: N 必须是正整数（收到「%s」）\n' "$N" >&2; exit 2 ;;
  esac
}

# ── 结果记录 ──────────────────────────────────────────────────────────────────
record() { # <PASS|FAIL|SKIP|DRY> <名称> <细节>
  local status="$1" name="$2" detail="${3:-}"
  printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
  printf '%s\t%s\t%s\n' "$status" "$name" "$detail" >>"$ITEMS_FILE"
  case "$status" in
    PASS) PASS_COUNT=$((PASS_COUNT + 1)) ;;
    FAIL) FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
    SKIP) SKIP_COUNT=$((SKIP_COUNT + 1)) ;;
    DRY) DRY_COUNT=$((DRY_COUNT + 1)) ;;
  esac
}

gate() { # <PASS|FAIL|SKIP|DRY> <名称> <细节>
  local status="$1" name="$2" detail="${3:-}"
  printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
  printf '%s\t%s\t%s\n' "$status" "$name" "$detail" >>"$GATES_FILE"
  case "$status" in
    FAIL) FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
    SKIP)
      # 门槛级 SKIP = 验收不完整：某条门槛从未被测量，main 末尾据此给出非零退出码
      SKIP_GATE_COUNT=$((SKIP_GATE_COUNT + 1))
      SKIP_GATE_NAMES="${SKIP_GATE_NAMES}${SKIP_GATE_NAMES:+、}$name"
      ;;
  esac
}

# ── 小工具 ────────────────────────────────────────────────────────────────────
have() { command -v "$1" >/dev/null 2>&1; }

first_line() { printf '%s' "${1%%$'\n'*}"; }

# interval 表达式 `<正整数><s|m|h>` → 秒；认不出的形状给空串（Demo C 的 note 判定用）
interval_secs_of() { # <expr>
  local expr="$1" num="" unit=""
  case "$expr" in
    *s|*m|*h) num="${expr%?}"; unit="${expr: -1}" ;;
    *) return 0 ;;
  esac
  case "$num" in ''|*[!0-9]*) return 0 ;; esac
  case "$unit" in
    s) printf '%s' "$((10#$num))" ;;
    m) printf '%s' "$((10#$num * 60))" ;;
    h) printf '%s' "$((10#$num * 3600))" ;;
  esac
}

# 展示用的端点：剥掉 URL 里的 user:pass@（端点偶尔带凭据，不落进结果文件）
redact_url() {
  printf '%s' "${1:-}" | sed -E 's#(//)[^/@]*@#\1***@#'
}

copy_tree() { # <src> <dst>：dst 先删后建
  rm -rf -- "$2"
  mkdir -p "$(dirname "$2")"
  cp -R "$1" "$2"
}

log_has() { [ -f "$1" ] && grep -qF -e "$2" -- "$1"; }

log_bytes() {
  if [ -f "$1" ]; then wc -c <"$1" | tr -d ' '; else printf '0'; fi
}

# JSONL 里 load_skill 实际加载过的技能名（去重、逗号分隔）
skills_in_log() {
  local out=""
  [ -f "$1" ] || { printf ''; return 0; }
  out="$(grep -o '"args":{"name":"[^"]*"' "$1" 2>/dev/null | sed 's/.*"name":"//;s/"$//' | sort -u | paste -sd, - 2>/dev/null || true)"
  printf '%s' "$out"
}

# ── 凭据 / 进程包装 ───────────────────────────────────────────────────────────
# 凭据只经环境变量注入；以下包装函数是唯一注入点，调用点不会再碰 key。
ignis_env_ws() { # <home> <workspace> <args...>（带 --workspace，可能走模型）
  local home="$1" ws="$2"
  shift 2
  env IGNIS_HOME="$home" IGNIS_WORKSPACE="$ws" \
    IGNIS_BASE_URL="${BASE_URL:-}" IGNIS_MODEL="${MODEL:-}" IGNIS_API_KEY="${API_KEY:-}" \
    "$IGNIS_BIN" --workspace "$ws" "$@"
}

run_ignis() { # <home> <ws> <stdout 文件> <stderr 文件> <run 参数...>
  local home="$1" ws="$2" out="$3" err="$4"
  shift 4
  if [ "$TIMEOUT_OK" = 1 ]; then
    env IGNIS_HOME="$home" IGNIS_WORKSPACE="$ws" \
      IGNIS_BASE_URL="${BASE_URL:-}" IGNIS_MODEL="${MODEL:-}" IGNIS_API_KEY="${API_KEY:-}" \
      timeout -k 10 "$RUN_TIMEOUT" "$IGNIS_BIN" --workspace "$ws" "$@" >"$out" 2>"$err"
  else
    env IGNIS_HOME="$home" IGNIS_WORKSPACE="$ws" \
      IGNIS_BASE_URL="${BASE_URL:-}" IGNIS_MODEL="${MODEL:-}" IGNIS_API_KEY="${API_KEY:-}" \
      "$IGNIS_BIN" --workspace "$ws" "$@" >"$out" 2>"$err"
  fi
}

ignis_cli() { # <home> <args...>（不带工作区的命令：doctor/skills/memory/daemon）
  local home="$1"
  shift
  env -u IGNIS_WORKSPACE IGNIS_HOME="$home" \
    IGNIS_BASE_URL="${BASE_URL:-}" IGNIS_MODEL="${MODEL:-}" IGNIS_API_KEY="${API_KEY:-}" \
    "$IGNIS_BIN" "$@"
}

# ── 数据库读取（证据核对） ────────────────────────────────────────────────────
# 直读 SQLite：验收判据是「库里真的留下了什么」，而不是 CLI 的渲染文本。
dbq() { # <db> <sql> → 行以 | 分隔
  local db="$1" sql="$2"
  [ -f "$db" ] || return 1
  if [ "$DB_BACKEND" = "sqlite3" ]; then
    sqlite3 -batch -noheader "$db" "$sql"
  else
    python3 - "$db" "$sql" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
try:
    rows = con.execute(sys.argv[2]).fetchall()
finally:
    con.close()
for row in rows:
    print("|".join("" if value is None else str(value) for value in row))
PY
  fi
}

q() { # <db> <sql> → 单行标量（查不到/出错给空串）
  local out=""
  out="$(dbq "$1" "$2" 2>/dev/null || true)"
  printf '%s' "${out%%$'\n'*}"
}

count() { # <db> <sql> → 非负整数（查不到给 0）
  local value=""
  value="$(q "$1" "$2")"
  case "$value" in
    ''|*[!0-9]*) printf '0' ;;
    *) printf '%s' "$value" ;;
  esac
}

# 否定侧断言专用（「没有重复」「没有 running 行」「没有孤儿行」）：查询失败必须与「零行」分开。
# q()/count() 会把查询失败吞成 ""/0——在否定侧恰好等于 PASS，属于危险方向。这里让失败显式非零
# 并打印原因到 stderr，调用方必须把它记成 FAIL（「查不到」≠「没有违规」）。
count_checked() { # <db> <sql> → 非负整数；库不存在/打不开/SQL 报错/非数字 → 非零
  local value=""
  if [ ! -f "$1" ]; then
    printf 'error: 否定侧检查失败：数据库不存在（%s）\n' "$1" >&2
    return 1
  fi
  if ! value="$(dbq "$1" "$2")"; then
    printf 'error: 否定侧检查失败：SQL 查询报错（db=%s）\n' "$1" >&2
    return 1
  fi
  case "$value" in
    ''|*[!0-9]*)
      printf 'error: 否定侧检查失败：SQL 未返回非负整数（db=%s）\n' "$1" >&2
      return 1
      ;;
  esac
  printf '%s' "$value"
}

# 日志/工作区里是否混入了 API key 原文（0 干净 / 1 泄漏 / 2 没有 python3 无法判定）
key_scan() { # <目录>
  local out="" rc=0
  [ -n "${API_KEY:-}" ] || return 0
  if ! have python3; then return 2; fi
  out="$(python3 - "$1" <<'PY'
import os, sys
key = os.environ.get("IGNIS_API_KEY", "")
root = sys.argv[1]
if not key:
    sys.exit(0)
hits = []
for dirpath, _dirnames, filenames in os.walk(root):
    for name in filenames:
        path = os.path.join(dirpath, name)
        try:
            with open(path, "rb") as handle:
                if key.encode() in handle.read():
                    hits.append(path)
        except OSError:
            pass
if hits:
    print("\n".join(hits))
    sys.exit(1)
PY
)" || rc=$?
  if [ "$rc" -eq 0 ]; then return 0; fi
  printf '%s\n' "$out" >&2
  return 1
}

check_key_scan() { # <名称> <目录>
  local name="$1" dir="$2" rc=0
  key_scan "$dir" >/dev/null 2>&1 || rc=$?
  case "$rc" in
    0) record PASS "$name" "日志与工作区内未出现 API key 原文" ;;
    1) record FAIL "$name" "日志或工作区内出现 API key 原文（路径已打到 stderr）" ;;
    *) record SKIP "$name" "本机没有 python3，无法做 key 原文扫描" ;;
  esac
}

# ── 守护进程清理 ─────────────────────────────────────────────────────────────
stop_daemon_hard() { # <home>：先走 daemon stop（幂等），仍活着就按 pidfile 收尸
  local home="$1" pid="" status_out="" i=""
  [ -n "${IGNIS_BIN:-}" ] || return 0
  [ -d "$home" ] || return 0
  ignis_cli "$home" daemon stop >/dev/null 2>&1 || true
  status_out="$(ignis_cli "$home" daemon status 2>/dev/null || true)"
  case "$status_out" in
    *"daemon running"*)
      pid="$(cat "$home/ignis.pid" 2>/dev/null || true)"
      case "$pid" in
        ''|*[!0-9]*) ;;
        *)
          log "强制收尸守护进程 pid=$pid（home=$home）"
          kill -TERM "$pid" 2>/dev/null || true
          for i in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
          done
          kill -KILL "$pid" 2>/dev/null || true
          ;;
      esac
      ;;
  esac
  return 0
}

# 退出码捕获：正常路径（main 的 exit / `set -e`）由 EXIT trap 原样带出；INT/TERM 路径显式给
# 128+信号——中断时 `$?` 可能停在 0，不能让「没验证完的中断」伪装成退出 0。
cleanup() { # <退出码>（省略时取 $?）
  local code="${1:-$?}"
  trap - EXIT INT TERM
  # 清理顺序不变：先按 pidfile 停掉自己拉起的守护进程，再处理临时目录
  if [ -n "${TRACK_FILE:-}" ] && [ -f "$TRACK_FILE" ]; then
    while IFS= read -r home; do
      [ -n "$home" ] || continue
      stop_daemon_hard "$home"
    done <"$TRACK_FILE"
  fi
  if [ "$KEEP" = 1 ]; then
    log "保留临时目录：${RUN_ROOT:-<未创建>}"
  elif [ -n "${RUN_ROOT:-}" ] && [ -d "$RUN_ROOT" ]; then
    rm -rf -- "$RUN_ROOT"
  fi
  exit "$code"
}

track_home() { printf '%s\n' "$1" >>"$TRACK_FILE"; }

# ── 前置检查 ─────────────────────────────────────────────────────────────────
PREFLIGHT_PROBLEMS=""
problem() { PREFLIGHT_PROBLEMS="${PREFLIGHT_PROBLEMS}- $1"$'\n'; }

preflight() {
  # 0) CI 守卫：这是手动工具（真实模型调用按量计费），默认不进 CI
  if [ "$DRY_RUN" = 0 ] && [ -n "${CI:-}" ] && [ "${IGNIS_ACCEPT_ALLOW_CI:-0}" != "1" ]; then
    printf 'error: 检测到 CI 环境（CI=%s）：验收 harness 是手动、可选工具，不进 CI。\n' "$CI" >&2
    printf '       确实要在 CI 里跑（自备真实模型凭据）请显式设置 IGNIS_ACCEPT_ALLOW_CI=1。\n' >&2
    return 1
  fi

  # 1) 凭据：真实运行缺一即拒绝；dry-run 不检查（也不联网）
  if [ "$DRY_RUN" = 1 ]; then
    log "dry-run：跳过 IGNIS_BASE_URL / IGNIS_MODEL / IGNIS_API_KEY 检查"
  else
    [ -n "${IGNIS_BASE_URL:-}" ] || problem "IGNIS_BASE_URL 未设置（任意 OpenAI 兼容端点，例如 https://your-endpoint/v1）"
    [ -n "${IGNIS_MODEL:-}" ] || problem "IGNIS_MODEL 未设置（端点上的模型名）"
    [ -n "${IGNIS_API_KEY:-}" ] || problem "IGNIS_API_KEY 未设置（密钥只经环境变量传入；harness 不落盘、不入仓库）"
  fi
  BASE_URL="${IGNIS_BASE_URL:-}"
  MODEL="${IGNIS_MODEL:-}"
  API_KEY="${IGNIS_API_KEY:-}"

  # 2) ignis 二进制
  if [ -n "${IGNIS_BIN:-}" ]; then
    [ -x "$IGNIS_BIN" ] || problem "IGNIS_BIN 指向的文件不可执行：$IGNIS_BIN"
  else
    local candidate=""
    for candidate in "$REPO_ROOT/target/release/ignis" "$REPO_ROOT/target/debug/ignis"; do
      if [ -x "$candidate" ]; then
        IGNIS_BIN="$candidate"
        break
      fi
    done
    if [ -z "$IGNIS_BIN" ]; then
      IGNIS_BIN="$(command -v ignis || true)"
    fi
    [ -n "$IGNIS_BIN" ] || problem "找不到 ignis 二进制（先 cargo build --release，或用 IGNIS_BIN 指定）"
  fi

  # 3) 夹具
  [ -f "$EXAMPLES_DIR/broken-project/package.json" ] || problem "缺夹具 examples/broken-project/package.json"
  [ -d "$EXAMPLES_DIR/test-server" ] || problem "缺夹具 examples/test-server/"
  [ -f "$EXAMPLES_DIR/uv-project/pyproject.toml" ] || problem "缺夹具 examples/uv-project/pyproject.toml"
  [ -f "$TASKS_FILE" ] || problem "缺任务清单 harness/tasks.md"

  # 4) 本机工具
  have npm || problem "本机找不到 npm（Demo A 与任务 #7 需要；Node ≥ 18）"
  if have sqlite3; then
    DB_BACKEND="sqlite3"
  elif have python3; then
    DB_BACKEND="python3"
  else
    problem "需要 sqlite3 或 python3 直读 ignis.db 核对证据"
  fi
  if have timeout && timeout -k 1 1 true >/dev/null 2>&1; then TIMEOUT_OK=1; fi
  if ! have uv; then
    log "uv 未安装：Demo B 会记为 SKIP（真实演示需要 uv：curl -LsSf https://astral.sh/uv/install.sh | sh）"
  fi

  # 5) doctor 冒烟（真实运行）：凭据与端点先过一遍自检，失败即拒绝启动
  if [ "$DRY_RUN" = 0 ] && [ -z "$PREFLIGHT_PROBLEMS" ]; then
    local dhome="$RUN_ROOT/preflight-home" dws="$RUN_ROOT/preflight-ws" dout="" drc=0
    mkdir -p "$dhome" "$dws"
    dout="$(ignis_env_ws "$dhome" "$dws" doctor 2>&1)" || drc=$?
    printf '%s\n' "$dout"
    DOCTOR_RC="$drc"
    if [ "$drc" -ne 0 ]; then
      problem "ignis doctor 退出码 $drc（逐行自检见上；先修好环境再跑验收，不要把它当成通过）"
    fi
  fi

  if [ -n "$PREFLIGHT_PROBLEMS" ]; then
    printf 'error: 前置检查未通过：\n%s' "$PREFLIGHT_PROBLEMS" >&2
    printf '提示：export IGNIS_BASE_URL=https://your-endpoint/v1 IGNIS_MODEL=your-model IGNIS_API_KEY=sk-... 后重试。\n' >&2
    return 1
  fi
  return 0
}

# ── 轮询 ─────────────────────────────────────────────────────────────────────
wait_for() { # <秒> <描述> <命令...>
  local timeout="$1" what="$2"
  shift 2
  local deadline=$((SECONDS + timeout))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if "$@"; then return 0; fi
    sleep 2
  done
  log "等待超时：$what"
  return 1
}

wait_for_rc() { # <秒> <描述> <命令...>：0=条件满足；1=超时；其他=命令自身报错（立即中止等待）
  # 给否定侧等待用：命令用 2 表示「查询失败」，不能与「条件暂时不满足（1）」混为一谈。
  local timeout="$1" what="$2"
  shift 2
  local deadline=$((SECONDS + timeout)) rc=1
  while [ "$SECONDS" -lt "$deadline" ]; do
    rc=0
    "$@" || rc=$?
    [ "$rc" -eq 1 ] || return "$rc"
    sleep 2
  done
  log "等待超时：$what"
  return "$rc"
}

# ── 通用证据（任务行 / 审批 / 用量 / JSONL） ─────────────────────────────────
# 输出 "id|status|kind|error|in|out|approvals|tools"；查不到给空串
task_snapshot() { # <db> <task_id>
  local db="$1" id="$2"
  [ -f "$db" ] || return 0
  printf '%s|%s|%s|%s|%s|%s|%s|%s' \
    "$(q "$db" "SELECT id FROM tasks WHERE id='$id'")" \
    "$(q "$db" "SELECT status FROM tasks WHERE id='$id'")" \
    "$(q "$db" "SELECT kind FROM tasks WHERE id='$id'")" \
    "$(q "$db" "SELECT COALESCE(error,'') FROM tasks WHERE id='$id'")" \
    "$(q "$db" "SELECT input_tokens FROM tasks WHERE id='$id'")" \
    "$(q "$db" "SELECT output_tokens FROM tasks WHERE id='$id'")" \
    "$(count "$db" "SELECT COUNT(*) FROM approvals WHERE task_id='$id'")" \
    "$(q "$db" "SELECT COALESCE(GROUP_CONCAT(tool),'') FROM approvals WHERE task_id='$id'")"
}

# 任务 JSONL 的存在性、收尾行与字节数：字节数进 JSONL_BYTES，问题追加到 FAILURES
check_task_log() { # <home> <task_id> <日志名词>
  local home="$1" id="$2" label="$3"
  local logpath="$home/logs/$id.jsonl"
  JSONL_BYTES=0
  if [ ! -f "$logpath" ]; then
    FAILURES="${FAILURES}${label} 的 JSONL 缺失（$logpath）; "
    return 0
  fi
  if ! log_has "$logpath" '"type":"task_end"'; then
    FAILURES="${FAILURES}${label} 的 JSONL 缺收尾行 task_end; "
  fi
  JSONL_BYTES="$(log_bytes "$logpath")"
  return 0
}

# ── Demo A：修好坏项目 ───────────────────────────────────────────────────────
demo_a_once() {
  local i="$1"
  local root="$RUN_ROOT/demo-a-$i" home="$RUN_ROOT/demo-a-$i/home" ws="$RUN_ROOT/demo-a-$i/ws"
  local name="demo-A #$i"
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "$name" "计划：cp -R examples/broken-project <ws>; npm test（须先失败）; ignis --workspace <ws> run \"$PROMPT_A\" --yes; npm test（须通过）; 核对 tasks/approvals/tokens/JSONL"
    return 0
  fi
  mkdir -p "$home"
  copy_tree "$EXAMPLES_DIR/broken-project" "$ws"
  track_home "$home"
  JSONL_BYTES=0 # 本次场景没用到的日志字节数不许沿用上一个场景的读数

  local FAILURES="" skills=""
  # 1) 先复现失败（夹具初始状态必须测试不过）
  local pre_rc=0
  (cd "$ws" && npm test >"$root/npm-before.log" 2>&1) || pre_rc=$?
  if [ "$pre_rc" -eq 0 ]; then
    FAILURES="${FAILURES}夹具初始 npm test 已通过（无法验收修复效果）; "
  fi

  # 2) 真实运行
  local rc=0
  run_ignis "$home" "$ws" "$root/run.out" "$root/run.err" run "$PROMPT_A" --yes || rc=$?
  local task_id="" snapshot="" status="" kind="" error="" tin="" tout="" approvals="" tools=""
  task_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  snapshot="$(task_snapshot "$home/ignis.db" "$task_id")"
  IFS='|' read -r _ status kind error tin tout approvals tools <<<"$snapshot"

  # 3) 修复后的复跑
  local post_rc=0
  (cd "$ws" && npm test >"$root/npm-after.log" 2>&1) || post_rc=$?

  # 4) 断言
  [ "$rc" -eq 0 ] || FAILURES="${FAILURES}run 退出码 $rc; "
  [ "$status" = "success" ] || FAILURES="${FAILURES}task status=${status:-<无任务行>}; "
  [ "$post_rc" -eq 0 ] || FAILURES="${FAILURES}修复后 npm test 退出 $post_rc; "
  if [ "${approvals:-0}" -lt 1 ]; then
    FAILURES="${FAILURES}没有审批记录（写操作应留痕）; "
  fi
  case "$tools" in
    *fs_edit*|*fs_write*) ;;
    *) FAILURES="${FAILURES}审批记录里没有写操作（tools=${tools:-none}）; " ;;
  esac
  if [ "$(( ${tin:-0} + ${tout:-0} ))" -le 0 ]; then
    FAILURES="${FAILURES}token 记账为 0（native 应从 API usage 记账）; "
  fi
  check_task_log "$home" "$task_id" "demo-A #$i"
  skills="$(skills_in_log "$home/logs/$task_id.jsonl")"
  local ks=0
  key_scan "$home" >/dev/null 2>&1 || ks=$?
  if [ "$ks" -eq 1 ]; then FAILURES="${FAILURES}日志里出现 API key 原文; "; fi

  local detail="exit=$rc status=${status:-none} approvals=${approvals:-0}(${tools:-none}) tokens=${tin:-0}/${tout:-0} jsonl=${JSONL_BYTES}B skills=${skills:-none}"
  if [ -z "$FAILURES" ]; then
    record PASS "$name" "$detail"
    A_PASS=$((A_PASS + 1))
  else
    record FAIL "$name" "$detail ${FAILURES}"
    A_FAIL=$((A_FAIL + 1))
  fi
  return 0
}

# ── Demo B：跨进程记忆（uv 未安装时记 SKIP） ─────────────────────────────────
demo_b_once() {
  local i="$1"
  local root="$RUN_ROOT/demo-b-$i" home="$RUN_ROOT/demo-b-$i/home" ws="$RUN_ROOT/demo-b-$i/ws"
  local name="demo-B #$i"
  if [ "$DRY_RUN" = 1 ]; then
    if have uv; then
      record DRY "$name" "计划：cp -R examples/uv-project <ws>; run \"$PROMPT_B_REMEMBER\" --yes（写记忆）; 新进程 run \"$PROMPT_B_REUSE\" --yes（须命中 uv 工作流）; run \"$PROMPT_B_CORRECT\" --yes（旧值 superseded）"
    else
      record DRY "$name" "本机无 uv：真实运行会记 SKIP（原因：真实演示需要 uv）"
    fi
    return 0
  fi
  mkdir -p "$home"
  copy_tree "$EXAMPLES_DIR/uv-project" "$ws"
  track_home "$home"
  JSONL_BYTES=0 # 同上：避免跨场景沿用读数

  local FAILURES=""
  # 会话 1：写入记忆
  local rc1=0
  run_ignis "$home" "$ws" "$root/stage1.out" "$root/stage1.err" run "$PROMPT_B_REMEMBER" --yes || rc1=$?
  local stage1_id="" stage1_status=""
  stage1_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  stage1_status="$(q "$home/ignis.db" "SELECT status FROM tasks WHERE id='$stage1_id'")"
  [ "$rc1" -eq 0 ] || FAILURES="${FAILURES}会话1 退出码 $rc1; "
  [ "$stage1_status" = "success" ] || FAILURES="${FAILURES}会话1 status=$stage1_status; "
  if [ "$(count "$home/ignis.db" "SELECT COUNT(*) FROM facts WHERE status='active' AND text LIKE '%uv%'")" -lt 1 ]; then
    FAILURES="${FAILURES}库里没有 uv 事实（remember 未落库）; "
  fi

  # 会话 2（新进程）：注入命中 → 选择 uv 工作流
  local rc2=0
  run_ignis "$home" "$ws" "$root/stage2.out" "$root/stage2.err" run "$PROMPT_B_REUSE" --yes || rc2=$?
  local stage2_id="" stage2_status="" injected="no"
  stage2_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  stage2_status="$(q "$home/ignis.db" "SELECT status FROM tasks WHERE id='$stage2_id'")"
  if log_has "$home/logs/$stage2_id.jsonl" "uv sync" || log_has "$home/logs/$stage2_id.jsonl" "uv run"; then
    injected="yes"
  fi
  [ "$rc2" -eq 0 ] || FAILURES="${FAILURES}会话2 退出码 $rc2; "
  [ "$injected" = "yes" ] || FAILURES="${FAILURES}会话2 未出现 uv 工作流命令（记忆注入未命中）; "
  check_task_log "$home" "$stage2_id" "demo-B #$i 会话2"

  # 会话 3（新进程）：纠正 → 旧 uv 值 superseded
  local rc3=0
  run_ignis "$home" "$ws" "$root/stage3.out" "$root/stage3.err" run "$PROMPT_B_CORRECT" --yes || rc3=$?
  local stage3_id="" stage3_status="" corrected="no"
  stage3_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  stage3_status="$(q "$home/ignis.db" "SELECT status FROM tasks WHERE id='$stage3_id'")"
  [ "$rc3" -eq 0 ] || FAILURES="${FAILURES}纠正会话 退出码 $rc3; "
  [ "$stage3_status" = "success" ] || FAILURES="${FAILURES}纠正会话 status=$stage3_status; "
  local superseded_uv="" active_pip=""
  superseded_uv="$(count "$home/ignis.db" "SELECT COUNT(*) FROM facts WHERE status='superseded' AND text LIKE '%uv%'")"
  active_pip="$(count "$home/ignis.db" "SELECT COUNT(*) FROM facts WHERE status='active' AND text LIKE '%pip%'")"
  if [ "$superseded_uv" -ge 1 ] && [ "$active_pip" -ge 1 ]; then corrected="yes"; fi
  [ "$superseded_uv" -ge 1 ] || FAILURES="${FAILURES}旧 uv 事实未被 superseded; "
  [ "$active_pip" -ge 1 ] || FAILURES="${FAILURES}没有 pip 新事实; "
  local ks=0
  key_scan "$home" >/dev/null 2>&1 || ks=$?
  if [ "$ks" -eq 1 ]; then FAILURES="${FAILURES}日志里出现 API key 原文; "; fi

  local detail="会话1=success 注入uv=${injected} 纠正=${corrected} 会话2 status=${stage2_status:-none}"
  if [ -z "$FAILURES" ]; then
    record PASS "$name" "$detail"
    B_PASS=$((B_PASS + 1))
  else
    record FAIL "$name" "$detail ${FAILURES}"
    B_FAIL=$((B_FAIL + 1))
  fi
  return 0
}

# ── Demo C：守护进程在后台主动工作 ───────────────────────────────────────────
c_has_terminal_run() { # <db> <job_id>
  local status=""
  status="$(q "$1" "SELECT status FROM job_runs WHERE job_id='$2' ORDER BY started_at DESC, id DESC LIMIT 1")"
  case "$status" in
    success|failed|aborted|limit_reached) return 0 ;;
    *) return 1 ;;
  esac
}

demo_c_once() {
  local i="$1"
  local root="$RUN_ROOT/demo-c-$i" home="$RUN_ROOT/demo-c-$i/home" ws="$RUN_ROOT/demo-c-$i/ws"
  local name="demo-C #$i"
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "$name" "计划：cp -R examples/test-server <ws>/examples/; run \"$PROMPT_C\" --yes（建 job）; ignis daemon start; 等调度运行与告警（≤${C_WAIT}s）; ignis alerts; ignis schedule history; ignis daemon stop"
    return 0
  fi
  mkdir -p "$home" "$ws/examples"
  copy_tree "$EXAMPLES_DIR/test-server" "$ws/examples/test-server"
  track_home "$home"
  JSONL_BYTES=0 # 同上：避免跨场景沿用读数

  local FAILURES="" skills=""
  # 1) 创建（CLI 进程；schedule_create 是 Elevated，--yes 预授权并留审批痕）
  local rc=0
  run_ignis "$home" "$ws" "$root/create.out" "$root/create.err" run "$PROMPT_C" --yes || rc=$?
  local create_id="" create_status="" create_ended=""
  create_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  create_status="$(q "$home/ignis.db" "SELECT status FROM tasks WHERE id='$create_id'")"
  create_ended="$(q "$home/ignis.db" "SELECT COALESCE(ended_at,0) FROM tasks WHERE id='$create_id'")"
  [ "$rc" -eq 0 ] || FAILURES="${FAILURES}创建 run 退出码 $rc; "
  [ "$create_status" = "success" ] || FAILURES="${FAILURES}创建任务 status=$create_status; "
  if [ "$(count "$home/ignis.db" "SELECT COUNT(*) FROM approvals WHERE task_id='$create_id' AND tool='schedule_create'")" -lt 1 ]; then
    FAILURES="${FAILURES}schedule_create 没有审批留痕; "
  fi
  local job_id="" job_status="" job_kind="" job_expr="" job_name="" job_prompt="" job_simulate="no"
  job_id="$(q "$home/ignis.db" "SELECT id FROM jobs ORDER BY created_at DESC, id DESC LIMIT 1")"
  job_status="$(q "$home/ignis.db" "SELECT status FROM jobs WHERE id='$job_id'")"
  job_kind="$(q "$home/ignis.db" "SELECT schedule_kind FROM jobs WHERE id='$job_id'")"
  job_expr="$(q "$home/ignis.db" "SELECT schedule_expr FROM jobs WHERE id='$job_id'")"
  # 作业提示由「创建 run」里的模型自写（两段式链路）：只读回显名称与是否含模拟步骤，
  # 供 alert=0 失败时第一手归因——提示漏掉 `--simulate 92` 时观测值低于阈值，「不告警」是正确行为。
  job_name="$(q "$home/ignis.db" "SELECT COALESCE(name,'') FROM jobs WHERE id='$job_id'")"
  job_prompt="$(q "$home/ignis.db" "SELECT COALESCE(prompt,'') FROM jobs WHERE id='$job_id'")"
  case "$job_prompt" in *--simulate*92*) job_simulate="yes" ;; esac
  if [ -z "$job_id" ]; then
    FAILURES="${FAILURES}没有落库的调度任务; "
  else
    [ "$job_status" = "active" ] || FAILURES="${FAILURES}job status=$job_status; "
  fi

  # 2) 退出 CLI 之后才启动守护进程（CLI 进程已收尸）
  local dstart_rc=0 dstart_out=""
  dstart_out="$(ignis_cli "$home" daemon start 2>&1)" || dstart_rc=$?
  [ "$dstart_rc" -eq 0 ] || FAILURES="${FAILURES}daemon start 退出码 $dstart_rc（$(first_line "$dstart_out")）; "

  # 3) 等一轮调度运行收尾（首个到期 = 创建时刻 + 间隔；守护进程最多 30s 重扫一次）
  local run_id="" run_status="" run_task="" run_tin="" run_tout="" run_started="" dlog_tail=""
  if [ -n "$job_id" ]; then
    wait_for "$C_WAIT" "job $job_id 的一次终态运行" c_has_terminal_run "$home/ignis.db" "$job_id" || true
    run_id="$(q "$home/ignis.db" "SELECT id FROM job_runs WHERE job_id='$job_id' ORDER BY started_at DESC, id DESC LIMIT 1")"
    run_status="$(q "$home/ignis.db" "SELECT status FROM job_runs WHERE id='$run_id'")"
    run_task="$(q "$home/ignis.db" "SELECT COALESCE(task_id,'') FROM job_runs WHERE id='$run_id'")"
    run_started="$(q "$home/ignis.db" "SELECT started_at FROM job_runs WHERE id='$run_id'")"
    run_tin="$(q "$home/ignis.db" "SELECT input_tokens FROM job_runs WHERE id='$run_id'")"
    run_tout="$(q "$home/ignis.db" "SELECT output_tokens FROM job_runs WHERE id='$run_id'")"
  fi
  [ "${run_status:-}" = "success" ] || FAILURES="${FAILURES}调度运行 status=${run_status:-none}（等待 ${C_WAIT}s）; "
  if [ -z "${run_status:-}" ]; then
    # 一次都没触发：把守护进程日志尾部带上，否则临时目录一删就没法追因了
    dlog_tail="$(tail -n 3 "$home/logs/daemon.log" 2>/dev/null | tr '\n' ' ' | cut -c1-240 || true)"
    if [ -n "$dlog_tail" ]; then
      FAILURES="${FAILURES}daemon 日志尾部：${dlog_tail}; "
    fi
  fi
  if [ -n "$run_started" ] && [ "${create_ended:-0}" -gt 0 ] && [ "$run_started" -lt "$create_ended" ]; then
    FAILURES="${FAILURES}调度运行早于 CLI 进程结束（不是后台触发）; "
  fi

  # 4) 后台运行的证据：kind=schedule、真实任务行、token、JSONL、告警
  local run_kind=""
  if [ -n "$run_task" ]; then
    run_kind="$(q "$home/ignis.db" "SELECT kind FROM tasks WHERE id='$run_task'")"
    [ "$run_kind" = "schedule" ] || FAILURES="${FAILURES}调度任务 kind=${run_kind:-none}（应为 schedule）; "
    check_task_log "$home" "$run_task" "demo-C #$i 调度运行"
    skills="$(skills_in_log "$home/logs/$run_task.jsonl")"
  else
    FAILURES="${FAILURES}调度运行没有回填 task_id; "
  fi
  if [ "$(( ${run_tin:-0} + ${run_tout:-0} ))" -le 0 ]; then
    FAILURES="${FAILURES}调度运行 token 记账为 0; "
  fi
  local alert_count="0" alert_title=""
  alert_count="$(count "$home/ignis.db" "SELECT COUNT(*) FROM alerts WHERE source_job_id='$job_id'")"
  if [ "${alert_count:-0}" -lt 1 ]; then
    # 不落告警不一定是产品缺陷：提示由模型自写，漏掉模拟步骤时观测值低于阈值，「不告警」正确。
    FAILURES="${FAILURES}没有落库的告警（alert=0）; 提示: 先查作业提示是否包含 --simulate 92（--keep 后用 sqlite3 查 jobs.prompt）以及守护运行的 JSONL 是否调用 notify_user；作业提示漏掉模拟步骤时「不告警」是正确行为; "
  fi
  alert_title="$(q "$home/ignis.db" "SELECT COALESCE(title,'') FROM alerts WHERE source_job_id='$job_id' ORDER BY id ASC LIMIT 1")"
  local alerts_out=""
  alerts_out="$(ignis_cli "$home" alerts 2>/dev/null || true)"
  if [ -n "$alert_title" ] && ! printf '%s' "$alerts_out" | grep -qF -e "$alert_title"; then
    FAILURES="${FAILURES}ignis alerts 里看不到这条告警; "
  fi
  if [ -n "$job_id" ]; then
    local history_out=""
    history_out="$(ignis_cli "$home" schedule history "$job_id" --limit 10 2>/dev/null || true)"
    if [ -n "$run_id" ] && ! printf '%s' "$history_out" | grep -qF -e "$run_id"; then
      FAILURES="${FAILURES}ignis schedule history 里看不到这次运行; "
    fi
  fi

  # 5) 停守护进程，确认没有残留
  local stop_rc=0 status_out=""
  ignis_cli "$home" daemon stop >/dev/null 2>&1 || stop_rc=$?
  [ "$stop_rc" -eq 0 ] || FAILURES="${FAILURES}daemon stop 退出码 $stop_rc; "
  status_out="$(ignis_cli "$home" daemon status 2>/dev/null || true)"
  case "$status_out" in
    *"daemon running"*) FAILURES="${FAILURES}daemon stop 之后仍在运行; " ;;
  esac
  local ks=0
  key_scan "$home" >/dev/null 2>&1 || ks=$?
  if [ "$ks" -eq 1 ]; then FAILURES="${FAILURES}日志里出现 API key 原文; "; fi

  # 详情行记录 job 名、提示是否含模拟步骤（simulate=yes|no）与调度表达式；间隔 ≤ 重扫窗口（RESCAN_SECS）时，「>30s 的首次到期」路径
  # 不会被本次运行覆盖——表达式由模型自选，这里只记一条 note，不判 FAIL。
  local rescan_note="" every_secs=""
  if [ "$job_kind" = "interval" ]; then
    every_secs="$(interval_secs_of "$job_expr")"
    if [ -n "$every_secs" ] && [ "$every_secs" -le "$RESCAN_SECS" ]; then
      rescan_note=" note: 间隔 ${job_expr} ≤ ${RESCAN_SECS}s 重扫窗口，>30s 首次到期路径未被本次运行覆盖（模型自选表达式，不判 FAIL）"
    fi
  fi

  local detail="job=${job_name:-none} simulate=${job_simulate} expr=${job_kind:-none}/${job_expr:-none} run=${run_status:-none} kind=${run_kind:-none} alert=${alert_count:-0} tokens=${run_tin:-0}/${run_tout:-0} jsonl=${JSONL_BYTES}B skills=${skills:-none}${rescan_note}"
  if [ -z "$FAILURES" ]; then
    record PASS "$name" "$detail 停止后无残留"
    C_PASS=$((C_PASS + 1))
  else
    record FAIL "$name" "$detail ${FAILURES}"
    C_FAIL=$((C_FAIL + 1))
  fi
  return 0
}

# ── demos 模式 ───────────────────────────────────────────────────────────────
mode_demos() {
  # 技能目录（demo.md 清单 #5 的「发现」侧；「执行」侧在 tasks #12-14）
  local skills_home="$RUN_ROOT/skills-home" skills_out="" builtin=""
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "技能目录" "计划：ignis skills list（内置三技能须可见）"
  else
    mkdir -p "$skills_home"
    skills_out="$(ignis_cli "$skills_home" skills list 2>/dev/null || true)"
    builtin="$(printf '%s\n' "$skills_out" | grep -c 'builtin' || true)"
    if [ "${builtin:-0}" -ge 2 ]; then
      record PASS "技能目录" "builtin=${builtin}（server-health/project-analyzer/report-generator）"
    else
      record FAIL "技能目录" "内置技能可见数 builtin=${builtin:-0} < 2"
    fi
  fi

  local i=""
  for ((i = 1; i <= N; i++)); do demo_a_once "$i"; done

  if [ "$DRY_RUN" = 1 ]; then
    demo_b_once 1
  elif have uv; then
    for ((i = 1; i <= N; i++)); do demo_b_once "$i"; done
  else
    record SKIP "demo-B（全部 $N 次）" "uv 未安装：真实演示需要 uv（https://astral.sh/uv/install.sh）；SKIP 不等于通过"
  fi

  for ((i = 1; i <= N; i++)); do demo_c_once "$i"; done

  demo_gate "Demo A ×$N" "$A_PASS" "$A_FAIL"
  demo_gate "Demo B ×$N" "$B_PASS" "$B_FAIL"
  demo_gate "Demo C ×$N" "$C_PASS" "$C_FAIL"
}

demo_gate() { # <名称> <PASS 数> <FAIL 数>
  local name="$1" pass="$2" fail="$3"
  if [ "$DRY_RUN" = 1 ]; then
    gate DRY "$name" "dry-run：只演练流程，不计门槛"
    return 0
  fi
  if [ "$pass" = "0" ] && [ "$fail" = "0" ]; then
    gate SKIP "$name" "全部跳过（原因见上），未计入门槛；跳过 ≠ 通过 → 整体判「验收不完整」"
  elif [ "$fail" = "0" ] && [ "$pass" = "$N" ]; then
    gate PASS "$name" "$pass/$N 连续成功"
  else
    gate FAIL "$name" "$pass/$N 成功（门槛：连续 $N 次全成功）"
  fi
}

# ── tasks 模式 ───────────────────────────────────────────────────────────────
parse_tasks() {
  awk -F'|' '
    /^\|[[:space:]]*[0-9]+[[:space:]]*\|/ {
      for (i = 1; i <= NF; i++) {
        gsub(/^[[:space:]]+/, "", $i)
        gsub(/[[:space:]]+$/, "", $i)
      }
      gsub(/`/, "", $5) # 判定列里的 Markdown 反引号不属于判定语法；prompt/预期保持原文
      print $2 "\t" $3 "\t" $4 "\t" $5
    }' "$TASKS_FILE"
}

task_run_one() { # <编号> <prompt> <判定子句> <home> <ws> <root>
  local num="$1" prompt="$2" checks="$3" home="$4" ws="$5" root="$6"
  local name="task #$num" out="$root/run-$num.out" err="$root/run-$num.err"
  LAST_TASK_PASS=0
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "$name" "计划：run \"$prompt\" --yes；判定：[$checks]"
    LAST_TASK_PASS=1
    return 0
  fi

  local rc=0
  run_ignis "$home" "$ws" "$out" "$err" run "$prompt" --yes || rc=$?
  local task_id="" status="" tin="" tout="" approvals="" logpath="" started="" FAILURES=""
  task_id="$(q "$home/ignis.db" "SELECT id FROM tasks WHERE kind='run' ORDER BY started_at DESC, id DESC LIMIT 1")"
  status="$(q "$home/ignis.db" "SELECT status FROM tasks WHERE id='$task_id'")"
  tin="$(q "$home/ignis.db" "SELECT input_tokens FROM tasks WHERE id='$task_id'")"
  tout="$(q "$home/ignis.db" "SELECT output_tokens FROM tasks WHERE id='$task_id'")"
  approvals="$(count "$home/ignis.db" "SELECT COUNT(*) FROM approvals WHERE task_id='$task_id'")"
  started="$(q "$home/ignis.db" "SELECT started_at FROM tasks WHERE id='$task_id'")"
  logpath="$home/logs/$task_id.jsonl"

  # 隐式断言：日志存在且带收尾行
  if [ ! -f "$logpath" ]; then
    FAILURES="${FAILURES}JSONL 缺失; "
  elif ! log_has "$logpath" '"type":"task_end"'; then
    FAILURES="${FAILURES}JSONL 缺 task_end; "
  fi

  # 判定子句（tasks.md 第 4 列）必须非空：空/缺失 = FAIL。
  # 不允许只靠上面的隐式检查兜底成 PASS——否则未来某行忘写判定就会悄悄弱化自己。
  if [ -z "$(printf '%s' "$checks" | tr -d '[:space:]')" ]; then
    FAILURES="${FAILURES}判定子句为空（tasks.md 第 4 列缺失）; "
  fi

  local clause="" value=""
  local -a clauses=()
  IFS=';' read -r -a clauses <<<"$checks"
  for clause in "${clauses[@]}"; do
    clause="${clause#"${clause%%[![:space:]]*}"}"
    clause="${clause%"${clause##*[![:space:]]}"}"
    [ -n "$clause" ] || continue
    case "$clause" in
      exit=*)
        value="${clause#exit=}"
        if [ "$rc" != "$value" ]; then FAILURES="${FAILURES}期望 ${clause}（实际 $rc）; "; fi
        ;;
      log~=*)
        value="${clause#log~=}"
        if ! log_has "$logpath" "$value"; then FAILURES="${FAILURES}日志缺「$value」; "; fi
        ;;
      skill=*)
        value="${clause#skill=}"
        if ! log_has "$logpath" "load_skill" || ! log_has "$logpath" "\"name\":\"$value\""; then
          FAILURES="${FAILURES}技能未加载（$value）; "
        fi
        ;;
      file=*)
        value="${clause#file=}"
        if [ ! -s "$ws/$value" ]; then FAILURES="${FAILURES}文件缺失或为空（$value）; "; fi
        ;;
      fact~=*)
        value="${clause#fact~=}"
        if [ "$(count "$home/ignis.db" "SELECT COUNT(*) FROM facts WHERE status='active' AND text LIKE '%$value%'")" -le 0 ]; then
          FAILURES="${FAILURES}没有含「$value」的 active 事实; "
        fi
        ;;
      recall~=*)
        value="${clause#recall~=}"
        if ! ignis_cli "$home" memory search "$value" 2>/dev/null | grep -qF -e "$value"; then
          FAILURES="${FAILURES}memory search「$value」无命中; "
        fi
        ;;
      alert\>=*)
        value="${clause#alert>=}"
        if [ "$(count "$home/ignis.db" "SELECT COUNT(*) FROM alerts WHERE source_task_id='$task_id'")" -lt "$value" ]; then
          FAILURES="${FAILURES}本次任务告警数不足（期望 ≥$value）; "
        fi
        ;;
      job\>=*)
        value="${clause#job>=}"
        if [ "$(count "$home/ignis.db" "SELECT COUNT(*) FROM jobs WHERE created_at >= $started")" -lt "$value" ]; then
          FAILURES="${FAILURES}本次任务未创建调度任务（期望 ≥$value）; "
        fi
        ;;
      tokens\>0)
        if [ "$(( ${tin:-0} + ${tout:-0} ))" -le 0 ]; then FAILURES="${FAILURES}tokens=0; "; fi
        ;;
      *)
        FAILURES="${FAILURES}未知判定子句「$clause」; "
        ;;
    esac
  done

  local detail="exit=$rc status=${status:-none} tokens=${tin:-0}/${tout:-0} approvals=${approvals:-0}"
  if [ -z "$FAILURES" ]; then
    record PASS "$name" "$detail"
    LAST_TASK_PASS=1
  else
    record FAIL "$name" "$detail ${FAILURES}"
    LAST_TASK_PASS=0
  fi
  return 0
}

gate_on_rate() { # <pass> <fail> <total>
  local pass="$1" fail="$2" total="$3" denominator=$(( $1 + $2 )) rate=0 partial=""
  [ "$denominator" -gt 0 ] && rate=$(( pass * 100 / denominator ))
  [ "$total" -lt 20 ] && partial="（N=$total < 20，不是完整门槛）"
  if [ "$denominator" -eq 0 ]; then
    gate SKIP "tasks 门槛 ≥${TASKS_GATE_PCT}%" "没有可统计的任务（全部跳过）"
  elif [ "$rate" -ge "$TASKS_GATE_PCT" ]; then
    gate PASS "tasks 门槛 ≥${TASKS_GATE_PCT}%" "${pass}/${denominator} = ${rate}%${partial}"
  else
    gate FAIL "tasks 门槛 ≥${TASKS_GATE_PCT}%" "${pass}/${denominator} = ${rate}% 低于 ${TASKS_GATE_PCT}%${partial}"
  fi
  # 日志脱敏（tasks 模式所有 home 都在同一个临时目录下）
  check_key_scan "tasks 日志脱敏" "$RUN_ROOT/tasks"
}

mode_tasks() {
  local root="$RUN_ROOT/tasks" home="$RUN_ROOT/tasks/home" ws="$RUN_ROOT/tasks/ws"
  if [ "$DRY_RUN" = 0 ]; then
    mkdir -p "$home"
    copy_tree "$EXAMPLES_DIR/broken-project" "$ws"
    mkdir -p "$ws/examples" "$ws/notes"
    copy_tree "$EXAMPLES_DIR/test-server" "$ws/examples/test-server"
    printf 'seed line\n' >"$ws/notes/seed.txt"
    track_home "$home"
  fi

  local rows="" num="" prompt="" expect="" checks=""
  rows="$(parse_tasks)"
  if [ -z "$rows" ]; then
    gate FAIL "tasks 门槛 ≥${TASKS_GATE_PCT}%" "harness/tasks.md 里没有解析到任务行"
    return 0
  fi

  local total=0 pass=0 failn=0
  while IFS=$'\t' read -r num prompt expect checks; do
    [ -n "$num" ] || continue
    [ "$total" -lt "$N" ] || break
    total=$((total + 1))
    task_run_one "$num" "$prompt" "$checks" "$home" "$ws" "$root"
    if [ "$LAST_TASK_PASS" = 1 ]; then
      pass=$((pass + 1))
    else
      failn=$((failn + 1))
    fi
  done <<<"$rows"

  if [ "$DRY_RUN" = 1 ]; then
    gate DRY "tasks 门槛 ≥${TASKS_GATE_PCT}%" "dry-run：解析到 $total 条，未执行"
    return 0
  fi
  gate_on_rate "$pass" "$failn" "$total"
}

# ── restarts 模式 ────────────────────────────────────────────────────────────
runs_now() { # <db> <job_id>
  dbq "$1" "SELECT id FROM job_runs WHERE job_id='$2'" 2>/dev/null || true
}

refresh_seen() { # <db> <job_id> <seen 文件>
  { cat "$3" 2>/dev/null || true; runs_now "$1" "$2"; } | awk 'NF' | sort -u >"$3.tmp"
  mv "$3.tmp" "$3"
}

no_running_runs() { # <db> <job_id>：0=已收尾；1=仍有 running；2=查询失败（不能当成「没有 running」）
  local n=""
  n="$(count_checked "$1" "SELECT COUNT(*) FROM job_runs WHERE job_id='$2' AND status='running'")" || return 2
  [ "$n" -eq 0 ]
}

mode_restarts() {
  local root="$RUN_ROOT/restarts" home="$RUN_ROOT/restarts/home" ws="$RUN_ROOT/restarts/ws"
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "restarts 准备" "计划：cp -R examples/test-server <ws>/examples/; run \"$PROMPT_RESTART\" --yes（建 20s 间隔任务）; daemon start"
    local d=""
    for ((d = 1; d <= N; d++)); do
      record DRY "restart #$d" "计划：daemon stop（须退出干净）→ daemon start → 核对 job_runs/tasks 无重复、无丢失"
    done
    gate DRY "restarts ×$N" "dry-run：只演练流程"
    return 0
  fi
  mkdir -p "$home" "$ws/examples"
  copy_tree "$EXAMPLES_DIR/test-server" "$ws/examples/test-server"
  track_home "$home"

  # 1) 建一个短间隔调度任务（20s），由真实模型调 schedule_create。
  #    刻意用 20s：重启周期必须真的产出新运行行；≥1m 的「从未运行」job 修复后也要 60s 才首次到期，
  #    等不起（>30s 首次到期路径由 gateway 判别测试与 demos 的 Demo C 覆盖，见 `PROMPT_RESTART` 处说明）。
  local rc=0
  run_ignis "$home" "$ws" "$root/create.out" "$root/create.err" run "$PROMPT_RESTART" --yes || rc=$?
  local job_id="" job_status="" job_expr="" job_kind=""
  job_id="$(q "$home/ignis.db" "SELECT id FROM jobs ORDER BY created_at DESC, id DESC LIMIT 1")"
  job_status="$(q "$home/ignis.db" "SELECT status FROM jobs WHERE id='$job_id'")"
  job_expr="$(q "$home/ignis.db" "SELECT schedule_expr FROM jobs WHERE id='$job_id'")"
  job_kind="$(q "$home/ignis.db" "SELECT schedule_kind FROM jobs WHERE id='$job_id'")"
  if [ "$rc" -eq 0 ] && [ "$job_status" = "active" ]; then
    record PASS "restarts 准备" "job=${job_id:0:8} kind=$job_kind expr=$job_expr status=active"
  else
    record FAIL "restarts 准备" "exit=$rc job_status=${job_status:-none}（创建 20s 调度任务失败）"
  fi

  # 2) 基线：先跑出一轮成功运行，后续重启才有「不重复/不丢失」的对象
  local dstart_rc=0 seen="$root/seen-runs.txt" prev_count=0
  ignis_cli "$home" daemon start >/dev/null 2>&1 || dstart_rc=$?
  [ "$dstart_rc" -eq 0 ] || record FAIL "restarts 基线" "daemon start 退出码 $dstart_rc"
  if [ -n "$job_id" ] && [ "$dstart_rc" -eq 0 ]; then
    if wait_for "$RESTART_BASELINE_WAIT" "基线调度运行" c_has_terminal_run "$home/ignis.db" "$job_id"; then
      record PASS "restarts 基线" "首轮运行已收尾（expr=$job_expr）"
    else
      record FAIL "restarts 基线" "等待 ${RESTART_BASELINE_WAIT}s 仍无终态运行"
    fi
  fi

  # 3) 连续启停 N 次，每次核对重复/丢失/孤儿
  : >"$seen"
  refresh_seen "$home/ignis.db" "$job_id" "$seen"
  prev_count="$(wc -l <"$seen" | tr -d ' ')"
  local i="" stop_out="" start_out="" status_out="" failures="" lost="" now_count=""
  local dup_fires=0 dup_tasks=0 orphans=0 dup_err=""
  for ((i = 1; i <= N; i++)); do
    failures=""
    stop_out="$(ignis_cli "$home" daemon stop 2>&1 || true)"
    status_out="$(ignis_cli "$home" daemon status 2>/dev/null || true)"
    case "$status_out" in
      *"daemon running"*) failures="${failures}stop 后仍在运行; " ;;
    esac
    start_out="$(ignis_cli "$home" daemon start 2>&1 || true)"
    case "$start_out" in
      *"daemon started"*) ;;
      *) failures="${failures}start 未成功（$(first_line "$start_out")）; " ;;
    esac
    sleep "$RESTART_SETTLE"

    # 重复/孤儿是否定侧断言：count_checked——查询失败不能下沉成 0（那是 PASS 方向）
    dup_fires=0 dup_tasks=0 orphans=0 dup_err=""
    dup_fires="$(count_checked "$home/ignis.db" "SELECT COUNT(*) FROM (SELECT 1 FROM job_runs WHERE job_id='$job_id' GROUP BY started_at HAVING COUNT(*) > 1)")" \
      || { dup_err="${dup_err}重复触发; "; dup_fires=0; }
    dup_tasks="$(count_checked "$home/ignis.db" "SELECT COUNT(*) FROM (SELECT 1 FROM job_runs WHERE task_id IS NOT NULL AND task_id <> '' GROUP BY task_id HAVING COUNT(*) > 1)")" \
      || { dup_err="${dup_err}重复引用; "; dup_tasks=0; }
    orphans="$(count_checked "$home/ignis.db" "SELECT COUNT(*) FROM job_runs WHERE task_id IS NOT NULL AND task_id <> '' AND task_id NOT IN (SELECT id FROM tasks)")" \
      || { dup_err="${dup_err}孤儿行; "; orphans=0; }
    if [ -n "$dup_err" ]; then
      R_DUP_ERR=$((R_DUP_ERR + 1))
      failures="${failures}${dup_err}查询失败（「查不到」不能当「零行」）; "
    fi
    # 跨轮累积取最大值：门槛行会抄进结果文档，绝不能被后来某轮的 0 覆盖掉先前的非零
    if [ "$dup_fires" -gt "$R_DUP_FIRES" ]; then R_DUP_FIRES="$dup_fires"; fi
    if [ "$dup_tasks" -gt "$R_DUP_TASKS" ]; then R_DUP_TASKS="$dup_tasks"; fi
    if [ "$orphans" -gt "$R_ORPHANS" ]; then R_ORPHANS="$orphans"; fi
    lost="$(comm -23 <(sort "$seen") <(runs_now "$home/ignis.db" "$job_id" | sort) | wc -l | tr -d ' ')"
    if [ "${lost:-0}" -gt 0 ]; then
      failures="${failures}丢失 $lost 条历史运行; "
    fi
    refresh_seen "$home/ignis.db" "$job_id" "$seen"
    now_count="$(wc -l <"$seen" | tr -d ' ')"
    if [ "$now_count" -lt "$prev_count" ]; then
      failures="${failures}运行数从 $prev_count 降到 $now_count; "
    fi
    prev_count="$now_count"
    [ "$dup_fires" -eq 0 ] || failures="${failures}同一时刻重复触发 $dup_fires 次; "
    [ "$dup_tasks" -eq 0 ] || failures="${failures}同一 task_id 被重复引用 $dup_tasks 次; "
    [ "$orphans" -eq 0 ] || failures="${failures}孤儿运行行 $orphans 条; "
    if [ -z "$failures" ]; then
      record PASS "restart #$i" "stop→start 干净；累计 runs=$now_count；重复触发=0 丢失=0"
      R_CYCLES_PASS=$((R_CYCLES_PASS + 1))
    else
      record FAIL "restart #$i" "累计 runs=$now_count ${failures}"
      R_CYCLES_FAIL=$((R_CYCLES_FAIL + 1))
    fi
  done

  # 4) 收尾：等在飞运行落终态，再停守护进程并确认无残留。
  #    「无 running 行」是否定侧断言：查询失败（rc≥2）与「等到超时仍有 running」（rc=1）分别记 FAIL，
  #    不能把「查不到」当「已收尾」。
  local drain_rc=0
  wait_for_rc 90 "所有运行收尾（无 running 行）" no_running_runs "$home/ignis.db" "$job_id" || drain_rc=$?
  case "$drain_rc" in
    0) : ;;
    1) record FAIL "restarts 收尾" "90s 内仍有 running 状态的运行行" ;;
    *) record FAIL "restarts 收尾" "running 行数查询失败：无法证明已收尾（「查不到」≠「零行」，见 stderr）" ;;
  esac
  ignis_cli "$home" daemon stop >/dev/null 2>&1 || true
  status_out="$(ignis_cli "$home" daemon status 2>/dev/null || true)"
  if [ -n "$job_id" ] && [ "$(q "$home/ignis.db" "SELECT status FROM jobs WHERE id='$job_id'")" != "active" ]; then
    record FAIL "restarts 收尾" "job 不再是 active"
  fi
  case "$status_out" in
    *"daemon running"*) record FAIL "restarts 收尾" "daemon stop 之后仍在运行（有残留）" ;;
    *) record PASS "restarts 收尾" "守护进程已停止，无残留；job 仍在" ;;
  esac

  local dup_total=$((R_DUP_FIRES + R_DUP_TASKS + R_ORPHANS))
  if [ "$R_CYCLES_FAIL" -eq 0 ] && [ "$R_CYCLES_PASS" -eq "$N" ] && [ "$dup_total" -eq 0 ] && [ "$R_DUP_ERR" -eq 0 ]; then
    gate PASS "restarts ×$N" "$R_CYCLES_PASS/$N 次启停干净；重复触发=0 重复引用=0 孤儿行=0 丢失=0"
  else
    gate FAIL "restarts ×$N" "$R_CYCLES_PASS/$N 通过；重复触发=$R_DUP_FIRES 重复引用=$R_DUP_TASKS 孤儿行=$R_ORPHANS 重复检查查询失败轮数=$R_DUP_ERR"
  fi
}

# ── 汇总与结果文件 ───────────────────────────────────────────────────────────
print_summary() {
  printf '\n== 逐项 ==\n'
  while IFS=$'\t' read -r status name detail; do
    printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
  done <"$ITEMS_FILE"
  printf '\n== 门槛 ==\n'
  while IFS=$'\t' read -r status name detail; do
    printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
  done <"$GATES_FILE"
  printf '\n合计：PASS=%d FAIL=%d SKIP=%d 门槛SKIP=%d DRY=%d（耗时 %ss）\n' \
    "$PASS_COUNT" "$FAIL_COUNT" "$SKIP_COUNT" "$SKIP_GATE_COUNT" "$DRY_COUNT" "$SECONDS"
}

# 结论判定（唯一来源）：main 的退出码与结果文件的结论行共用同一套口径，避免两处漂开。
verdict_rc() {
  if [ "$DRY_RUN" = 1 ]; then printf '0'; return 0; fi
  if [ "$FAIL_COUNT" -gt 0 ]; then printf '1'; return 0; fi
  if [ "$SKIP_GATE_COUNT" -gt 0 ]; then printf '1'; return 0; fi
  printf '0'
}

# 结论行（结果文件引用 <rc>）：与 verdict_rc 同一判定
verdict_line() { # <rc>
  case "$1" in
    0)
      if [ "$DRY_RUN" = 1 ]; then
        printf 'dry-run：没有执行任何断言（不是通过）'
      else
        printf '验收通过：PASS=%d FAIL=0 SKIP=%d' "$PASS_COUNT" "$SKIP_COUNT"
      fi
      ;;
    *)
      if [ "$FAIL_COUNT" -gt 0 ]; then
        printf '验收未通过：FAIL=%d（SKIP=%d 不算通过）' "$FAIL_COUNT" "$SKIP_COUNT"
      elif [ "$SKIP_GATE_COUNT" -gt 0 ]; then
        printf '验收不完整：%s 未运行（门槛 SKIP=%d）——跳过不等于通过' "${SKIP_GATE_NAMES:-某条门槛}" "$SKIP_GATE_COUNT"
      else
        printf '未走完汇总（中断或意外路径），按未验证处理'
      fi
      ;;
  esac
}

write_results() { # <退出码>：汇总 + 最终结论一并写盘（结论行与退出码同源）
  local rc="${1:-0}"
  if [ "$DRY_RUN" = 1 ]; then
    log "dry-run：未写结果文件（真实运行会写 $RESULTS_PATH）"
    return 0
  fi
  {
    printf '# Ignis 验收结果（%s）\n\n' "$START_TS"
    printf -- '- 端点：`%s`（密钥只经环境变量传入：不落盘、不入本文件）\n' "$(redact_url "$BASE_URL")"
    printf -- '- 模型：`%s`；内核：`native`；二进制：`%s`\n' "$MODEL" "$IGNIS_BIN"
    printf -- '- 模式：`%s`；N=%s；耗时：%ss\n' "$MODE" "$N" "$SECONDS"
    printf -- '- 临时数据目录（退出时已清理）：`%s`\n' "$RUN_ROOT"
    printf -- '- 门槛定义与 demo.md 清单映射：`docs/acceptance.md`\n'
    printf '\n## 门槛\n\n```\n'
    while IFS=$'\t' read -r status name detail; do
      printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
    done <"$GATES_FILE"
    printf '```\n\n## 逐项\n\n```\n'
    while IFS=$'\t' read -r status name detail; do
      printf '%-5s %-30s %s\n' "$status" "$name" "$detail"
    done <"$ITEMS_FILE"
    printf '```\n\n## 结论\n\n```\n'
    printf 'exit=%s %s\n' "$rc" "$(verdict_line "$rc")"
    printf '```\n\n## 说明\n\n'
    printf -- '- SKIP 不是 PASS：每一行 SKIP 都带原因（例如本机未安装 uv）；门槛级 SKIP 会把整体结论判成「验收不完整」。\n'
    printf -- '- 退出码 0 只在没有任何 FAIL 且门槛行没有 SKIP 时给出；门槛 SKIP（某条门槛从未被测量）⇒ 退出码 1。本文件由 `harness/acceptance.sh` 自动生成。\n'
    printf -- '- 上面「结论」行的 `exit=<码>` 与脚本实际退出码同源（`verdict_rc`），不是事后抄写。\n'
    printf -- '- 端点与模型来自环境变量；本文件只记录脱敏后的端点（URL 里的 user:pass@ 会被打码）。\n'
  } >"$RESULTS_PATH"
  log "结果已写入：$RESULTS_PATH"
}

# ── 主流程 ───────────────────────────────────────────────────────────────────
main() {
  parse_args "$@"
  RUN_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ignis-acceptance.XXXXXX")"
  ITEMS_FILE="$RUN_ROOT/items.tsv"
  GATES_FILE="$RUN_ROOT/gates.tsv"
  TRACK_FILE="$RUN_ROOT/daemon-homes.txt"
  : >"$ITEMS_FILE"
  : >"$GATES_FILE"
  : >"$TRACK_FILE"
  # 退出码捕获：EXIT 路径原样带出真实状态；INT/TERM 显式给 128+信号（中断不是通过）。
  # 两者都只经 cleanup（先停守护进程、再清临时目录）收尾。
  trap 'cleanup "$?"' EXIT
  trap 'cleanup 130' INT
  trap 'cleanup 143' TERM

  log "模式=$MODE N=$N$([ "$DRY_RUN" = 1 ] && printf '（dry-run）')；临时目录=$RUN_ROOT"
  if ! preflight; then
    record FAIL "前置检查（未通过）" "缺环境/缺工具/CI 环境等，具体原因见 stderr（退出码 2，不是通过）"
    print_summary
    # 前置检查没过就不写结果文件：本次没有任何场景被真正执行，别留下误读为「跑过了」的产物
    exit 2
  fi
  if [ "$DRY_RUN" = 1 ]; then
    record DRY "环境自检（doctor）" "dry-run：不调 doctor（不联网）"
  else
    record PASS "环境自检（doctor）" "exit=${DOCTOR_RC:-0}（逐行输出见上）"
  fi

  case "$MODE" in
    demos) mode_demos ;;
    tasks) mode_tasks ;;
    restarts) mode_restarts ;;
  esac

  print_summary
  local verdict=""
  verdict="$(verdict_rc)"
  write_results "$verdict"
  if [ "$DRY_RUN" = 1 ]; then
    log "dry-run 结束：流程演练完成，没有任何断言被执行（这不是通过）"
  elif [ "$FAIL_COUNT" -gt 0 ]; then
    log "验收未通过：FAIL=$FAIL_COUNT（SKIP=$SKIP_COUNT 不算通过）"
  elif [ "$SKIP_GATE_COUNT" -gt 0 ]; then
    # 门槛级 SKIP：某条门槛从未被测量（典型：无 uv 时 Demo B 整条跳过）——不是通过，也不是 FAIL
    log "验收不完整：${SKIP_GATE_NAMES:-某条门槛} 未运行（门槛 SKIP=$SKIP_GATE_COUNT）——跳过不等于通过，退出码 1"
  else
    log "验收通过：PASS=$PASS_COUNT SKIP=$SKIP_COUNT"
  fi
  exit "$verdict"
}

main "$@"
