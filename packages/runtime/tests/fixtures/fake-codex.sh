#!/bin/sh
# 假 codex CLI：逐字回放 `codex exec --json` 的真实事件形状
# （形状来源：docs/spikes/2026-10-15-external-kernels-probe.md）。
#
# 行为由最后一个位置参数（渲染好的 prompt）里的关键词决定：
#   TOOL          工具/命令事件（只观察，不执行）
#   TURNFAIL      turn.failed 终局（退出码 0，证明失败判定不看退出码）
#   FAIL          非零退出 + 两行 stderr
#   NOISE         未知事件类型 / 未知条目类型 / 非 JSON 行混在正常文本流里
#   CANCEL_HANG   打出前导事件后挂起（取消用例；后台 sleep 只能靠进程组收尸）
#   TIMEOUT_HANG  同上（超时用例）
#   其余          纯文本一轮成功
#
# 另外把收到的命令行写进工作区：开关逐行落在 .fake-codex-argv（最后一个是 prompt，
# 单独原样写进 .fake-codex-prompt，因为它自带换行），测试据此钉住 spawn 的参数与渲染。
set -eu

prompt=""
for arg in "$@"; do
  prompt="$arg"
done

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
*TURNFAIL*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"半截"}}'
  emit '{"type":"turn.failed","error":{"message":"model response stream ended unexpectedly"}}'
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
  sleep 12351 &
  sleep 12352
  ;;
*TIMEOUT_HANG*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"挂起前"}}'
  sleep 12361 &
  sleep 12362
  ;;
*)
  emit '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
  emit '{"type":"turn.started"}'
  emit '{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Confirming readiness for response**"}}'
  emit '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"OK."}}'
  emit '{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":5}}'
  ;;
esac
