#!/bin/sh
# 假 claude CLI：逐字回放 `claude -p --verbose --output-format stream-json` 的真实事件形状
# （形状来源：docs/spikes/2026-10-15-external-kernels-probe.md 的 claude 小节）。
#
# 行为由最后一个位置参数（渲染好的 prompt）里的关键词决定：
#   TOOL            assistant 里带 tool_use 块（只观察，不发 ToolResult）
#   NOISE           system/未知类型/未知内容块/非 JSON 行混在正常文本流里
#   RESULT_ERR      result.is_error == true（退出码 0，证明失败判定不看退出码）
#   RESULT_BARE     result.is_error == true 但不带 result 文本，之后挂起（证明终局一到手即收束、
#                   缺失原因也走「失败」而不是「没见到终局」或超时）
#   RESULT_NOFLAG   result 不带 is_error 字段（schema 漂移；按失败的安全默认识别）
#   COMPLETE_HANG   result 成功之后挂起（证明完成的轮次不被拖成超时）
#   STDERR_ORPHAN   result 成功之后正常退出，但留下一个攥着继承的 stderr 写端的后代进程
#                   （用 setsid 逃出进程组；证明 stderr 收尾必须有上限，见评审 #1）
#   STREAM_END      只吐前导事件就正常退出（流结束而未见终局）
#   FAIL            非零退出 + 两行 stderr（没有 result 终局）
#   REAL_FAIL       真机失败轨迹逐字回放（assistant 文本 = API 错误 + result.is_error + 退出码 1）
#   CANCEL_HANG     打出前导事件后挂起（取消用例；后台 sleep 只能靠进程组收尸）
#   TIMEOUT_HANG    同上（超时用例）
#   其余            纯文本一轮成功
#
# 另外把收到的命令行写进工作区：开关逐行落在 .fake-claude-argv（最后一个是 prompt，
# 单独原样写进 .fake-claude-prompt，因为它自带换行），测试据此钉住 spawn 的参数与渲染。
# 挂起分支把休眠哨兵号写进 .fake-claude-sleepers，测试据此断言没有残留进程。
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
  printf '%s\n' "$sentinel" "$((sentinel + 1))" > .fake-claude-sleepers
  sleep "$sentinel" &
  sleep "$((sentinel + 1))"
}

count=$#
index=0
: > .fake-claude-argv
: > .fake-claude-prompt
for arg in "$@"; do
  index=$((index + 1))
  if [ "$index" -lt "$count" ]; then
    printf '%s\n' "$arg" >> .fake-claude-argv
  else
    printf '%s' "$arg" >> .fake-claude-prompt
  fi
done

emit() { printf '%s\n' "$1"; }

# system/init 与 system/thinking_tokens 是真机每轮都有的前导行（后者还很多），全都不映射。
init_line='{"type":"system","subtype":"init","cwd":".","session_id":"00000000-0000-4000-8000-000000000001","model":"MiniMax-M2","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.274-fake"}'
tokens_line='{"type":"system","subtype":"thinking_tokens","estimated_tokens":2,"estimated_tokens_delta":2,"session_id":"00000000-0000-4000-8000-000000000001","uuid":"00000000-0000-4000-8000-000000000002"}'

case "$prompt" in
*RESULT_BARE*)
  emit "$init_line"
  emit '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1}'
  hang
  ;;
*RESULT_NOFLAG*)
  emit "$init_line"
  emit '{"type":"result","subtype":"success","num_turns":1,"result":"不该被当成成功"}'
  ;;
*RESULT_ERR*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"半截"}]}}'
  emit '{"type":"result","subtype":"success","is_error":true,"num_turns":1,"api_error_status":429,"terminal_reason":"api_error","result":"API Error: Request rejected (429)\n第二行不该出现"}'
  ;;
*COMPLETE_HANG*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"用户只是想聊一句"}]}}'
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"完成了但进程不退出"}]}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"完成了但进程不退出"}'
  hang
  ;;
# 后代进程攥住继承来的 stderr 写端：真机形态是 `Bash` 工具留下的后台进程、继承 fd 的
# MCP/helper 子进程。父进程正常退出（不触发进程组 SIGKILL），后代又用 setsid 逃出了本进程组
# ——只有有界的 stderr 收尾能救这一轮。后代活到工作区里出现 .fake-claude-stderr-release 为止
# （另有 60s 自愈上限），它的命令行里带着本次运行的哨兵号（`sh -c … <哨兵>`）。
*STDERR_ORPHAN*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"完成了但 stderr 写端还开着"}]}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"完成了但 stderr 写端还开着"}'
  printf '%s\n' "$sentinel" > .fake-claude-sleepers
  setsid sh -c 'i=0
    while [ ! -e .fake-claude-stderr-release ] && [ "$i" -lt 60 ]; do
      sleep 1
      i=$((i + 1))
    done' "$sentinel" &
  ;;
*STREAM_END*)
  emit "$init_line"
  emit "$tokens_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"话说到一半"}]}}'
  ;;
*REAL_FAIL*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"API Error: 400 Upstream request failed: Model is unavailable."}]}}'
  emit '{"type":"result","subtype":"success","is_error":true,"num_turns":1,"api_error_status":400,"terminal_reason":"api_error","result":"API Error: 400 Upstream request failed: Model is unavailable."}'
  exit 1
  ;;
*FAIL*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"半截"}]}}'
  printf '%s\n' 'claude: fake failure line 1' >&2
  printf '%s\n' 'claude: fake failure line 2' >&2
  exit 3
  ;;
*NOISE*)
  emit "$init_line"
  emit "$tokens_line"
  emit '{"type":"future.event","payload":{"n":1}}'
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"先想一下"},{"type":"text","text":"noise ok"}]}}'
  emit '{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"a.txt","is_error":false}]}}'
  emit 'not json at all'
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"future_block","text":"x"}]}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"noise ok"}'
  ;;
*TOOL*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"先看看目录"},{"type":"tool_use","id":"call_function_1","name":"Bash","input":{"command":"ls -la","description":"List files"}}]}}'
  emit '{"type":"user","message":{"role":"user","content":[{"tool_use_id":"call_function_1","type":"tool_result","content":"total 4\n-rw-rw-r-- 1 tobi tobi 6 a.txt","is_error":false}]}}'
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"列完了"}]}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"permission_denials":[],"result":"列完了"}'
  ;;
*CANCEL_HANG*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"挂起前"}]}}'
  hang
  ;;
*TIMEOUT_HANG*)
  emit "$init_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"挂起前"}]}}'
  hang
  ;;
*)
  emit "$init_line"
  emit "$tokens_line"
  emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"确认一下要说什么"},{"type":"text","text":"OK"}]}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"OK"}'
  ;;
esac
