#!/bin/sh
# 假 codex CLI：逐字回放 `codex exec --json` 的真实事件形状
# （形状来源：docs/spikes/2026-10-15-external-kernels-probe.md）。
#
# 行为由最后一个位置参数（渲染好的 prompt）里的关键词决定：
#   TOOL          工具/命令事件（只观察，不执行）
#   TURNFAIL      turn.failed 终局（退出码 0，证明失败判定不看退出码）
#   TURNFAIL_BARE turn.failed 不带 error.message，之后挂起（证明终局一到手即收束、
#                 缺失原因也走「失败」而不是「没见到终局」或超时）
#   COMPLETE_HANG turn.completed 之后挂起（证明完成的轮次不被拖成超时）
#   STDERR_ORPHAN turn.completed 之后正常退出，但留下一个攥着继承的 stderr 写端的后代进程
#                 （用 setsid 逃出进程组；证明 stderr 收尾必须有上限，见评审 #1）
#   FAIL          非零退出 + 两行 stderr
#   REAL_FAIL     真机失败轨迹逐字回放（非终局 `error` 行 + turn.failed + 退出码 1）
#   NOISE         未知事件类型 / 未知条目类型 / 非 JSON 行混在正常文本流里
#   CANCEL_HANG   打出前导事件后挂起（取消用例；后台 sleep 只能靠进程组收尸）
#   TIMEOUT_HANG  同上（超时用例）
#   其余          纯文本一轮成功
#
# 另外把收到的命令行写进工作区：开关逐行落在 .fake-codex-argv（最后一个是 prompt，
# 单独原样写进 .fake-codex-prompt，因为它自带换行），测试据此钉住 spawn 的参数与渲染。
# 挂起分支把休眠哨兵号写进 .fake-codex-sleepers，测试据此断言没有残留进程。
set -eu

prompt=""
for arg in "$@"; do
  prompt="$arg"
done

# 休眠哨兵：由工作区路径派生（每个用例一个新建的临时工作区，故每次运行都是新数字）——
# 固定数字会被上一次被 SIGKILL 的运行留下的孤儿进程误伤（评审 Minor #5）。
sentinel=$(printf '%s' "$PWD" | cksum)
sentinel=${sentinel%% *}
# 下限抬到 10 位：极小值（0/1）会让「挂起」变成一闪而过，测试就失去意义
sentinel=$((sentinel % 1000000000 + 1000000000))

# 挂起：后台 + 前台各一个哨兵进程（取消/超时用例只能靠进程组收尸）。哨兵号先写进 tag 文件，
# 测试据此断言这两个进程都已不在（用派生的数字，孤儿的残留不会误伤新一次运行）。
hang() {
  printf '%s\n' "$sentinel" "$((sentinel + 1))" > .fake-codex-sleepers
  sleep "$sentinel" &
  sleep "$((sentinel + 1))"
}

count=$#
index=0
: > .fake-codex-argv
: > .fake-codex-prompt
for arg in "$@"; do
  index=$((index + 1))
  if [ "$index" -lt "$count" ]; then
    printf '%s\n' "$arg" >> .fake-codex-argv
  else
    printf '%s' "$arg" >> .fake-codex-prompt
  fi
done

emit() { printf '%s\n' "$1"; }

case "$prompt" in
*TURNFAIL_BARE*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"turn.failed"}'
  hang
  ;;
*COMPLETE_HANG*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"完成了但进程不退出"}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":5}}'
  hang
  ;;
# 后代进程攥住继承来的 stderr 写端：真机形态是 `Bash` 工具留下的后台进程、继承 fd 的
# MCP/helper 子进程。父进程正常退出（不触发进程组 SIGKILL），后代又用 setsid 逃出了本进程组
# ——只有有界的 stderr 收尾能救这一轮。后代活到工作区里出现 .fake-codex-stderr-release 为止
# （另有 60s 自愈上限），它的命令行里带着本次运行的哨兵号（`sh -c … <哨兵>`）。
*STDERR_ORPHAN*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"完成了但 stderr 写端还开着"}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":5}}'
  printf '%s\n' "$sentinel" > .fake-codex-sleepers
  setsid sh -c 'i=0
    while [ ! -e .fake-codex-stderr-release ] && [ "$i" -lt 60 ]; do
      sleep 1
      i=$((i + 1))
    done' "$sentinel" &
  ;;
*TURNFAIL*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"半截"}}'
  emit '{"type":"turn.failed","error":{"message":"model response stream ended unexpectedly"}}'
  ;;
*REAL_FAIL*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"error","message":"invalid params, code: 2013, msg: invalid params, unknown model '"'"'no-such-model-xyz'"'"' (2013)"}'
  emit '{"type":"turn.failed","error":{"message":"invalid params, code: 2013, msg: invalid params, unknown model '"'"'no-such-model-xyz'"'"' (2013)"}}'
  exit 1
  ;;
*FAIL*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"error","message":"stream error: broken pipe"}}'
  printf '%s\n' 'codex: fake failure line 1' >&2
  printf '%s\n' 'codex: fake failure line 2' >&2
  exit 3
  ;;
*NOISE*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"future.event","payload":{"n":1}}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"todo_list","items":[{"text":"x","completed":true}]}}'
  emit 'not json at all'
  emit '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"noise ok"}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}'
  ;;
*TOOL*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"bash -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"bash -lc ls","aggregated_output":"docs\nsrc\n","exit_code":0,"status":"completed"}}'
  emit '{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"docs/x.md","kind":"add"}],"status":"completed"}}'
  emit '{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"列完了"}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"cache_write_input_tokens":0,"output_tokens":122,"reasoning_output_tokens":64}}'
  ;;
*CANCEL_HANG*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"挂起前"}}'
  hang
  ;;
*TIMEOUT_HANG*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"挂起前"}}'
  hang
  ;;
*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Confirming readiness for response**"}}'
  emit '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"OK."}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":5}}'
  ;;
esac
