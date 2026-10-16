#!/bin/sh
# test-server 健康检查夹具：打印一行 JSON，供 Demo C 的调度运行与手工联调使用。
#
# 用法：check-health.sh [--simulate <pct>]
#   --simulate <pct>  用给定百分比替代根分区的真实占用（演示/联调用，不依赖磁盘状态）
#
# 输出：{"disk_pct":<0-100>,"mem_pct":<0-100>,"docker":<true|false>}
#   disk_pct  根分区占用百分比；mem_pct  内存占用百分比（/proc/meminfo 读不到时为 0）；
#   docker    PATH 上有没有 docker。
set -eu

simulate=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --simulate)
      [ "$#" -ge 2 ] || {
        echo "check-health.sh: --simulate 需要一个百分比参数" >&2
        exit 2
      }
      simulate="$2"
      shift 2
      ;;
    *)
      echo "check-health.sh: 未知参数 $1（用法：check-health.sh [--simulate <pct>]）" >&2
      exit 2
      ;;
  esac
done

if [ -n "$simulate" ]; then
  case "$simulate" in
    [0-9] | [1-9][0-9] | 100) ;;
    *)
      echo "check-health.sh: --simulate 只接受 0-100 的整数，收到「$simulate」" >&2
      exit 2
      ;;
  esac
  disk_pct="$simulate"
else
  # df 的占用列自带 `%`：去掉它，只留数字
  disk_pct=$(df -Pk / | awk 'NR == 2 { gsub(/%/, "", $5); print $5 }')
fi

mem_pct=0
if [ -r /proc/meminfo ]; then
  mem_pct=$(awk '
    /^MemTotal:/     { total = $2 }
    /^MemAvailable:/ { avail = $2 }
    END { if (total > 0) printf "%d", (total - avail) * 100 / total }
  ' /proc/meminfo)
fi

docker=false
if command -v docker >/dev/null 2>&1; then
  docker=true
fi

printf '{"disk_pct":%s,"mem_pct":%s,"docker":%s}\n' "$disk_pct" "$mem_pct" "$docker"
