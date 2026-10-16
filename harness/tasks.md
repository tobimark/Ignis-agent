# Ignis 常见任务验收清单（20 例）

> `demo.md` 的内部门槛之一：**20 次常见任务测试的端到端成功率达到 80% 以上**。
> 本文件是这 20 例的唯一事实来源；`harness/acceptance.sh tasks [N]` 按顺序执行前 N 条
> （默认 20），逐条判定并统计成功率。门槛定义见 [`docs/acceptance.md`](../docs/acceptance.md)。

## 运行方式与判定口径

- 运行：`harness/acceptance.sh tasks [N]`（真实模型、手动、可选；不进 CI）。
- 工作区：临时目录里的 `examples/broken-project` 副本，另有 `examples/test-server/check-health.sh`
  与预置的 `notes/seed.txt`；`IGNIS_HOME` 为临时目录（**不碰真实的 `~/.ignis`**）。
- 每条任务是一条 `ignis --workspace <ws> run "<prompt>" --yes`：退出码 0 只说明「任务成功收尾」，
  不等于「工具真的做了事」——所以每条都带机器可判定的第 4 列。
- 顺序执行、共享同一份记忆与数据库：第 10 条写入的事实由第 11 条在新进程里检索，
  这正是 `demo.md` 验收清单 #4「重启后读取记忆」的量化形态。
- 判定子句（第 4 列，`;` 分隔，全部满足才算该条 PASS）：
  - `exit=N`：`ignis run` 的退出码等于 N
  - `log~=TEXT`：本次任务 JSONL（`IGNIS_HOME/logs/<task_id>.jsonl`）包含 TEXT
  - `skill=NAME`：JSONL 里出现 `load_skill` 调用且参数 `name` 为 NAME（技能被选择并加载）
  - `file=PATH`：工作区里 `PATH` 存在且非空
  - `fact~=TEXT`：`facts` 表里有文本包含 TEXT 的 active 事实
  - `recall~=TEXT`：`ignis memory search TEXT` 能命中（新进程里的检索路径）
  - `alert>=N`：本次任务产出的告警（`alerts.source_task_id = 任务 id`）≥ N
  - `job>=N`：本次任务创建的调度任务 ≥ N
  - `tokens>0`：本次任务有用量记账（native 内核来自 API usage）
- 每条任务还隐含两条：JSONL 存在且含收尾行 `task_end`；工作区与日志里不出现 API key 原文。

| # | 任务（ignis run 的 prompt） | 预期结果 | 判定 |
|---|---------------------------|---------|------|
| 1 | 读取 src/config.js，用一句话说明它的作用。 | 真实读文件（fs_read），任务成功 | `exit=0;log~=fs_read` |
| 2 | 列出当前工作区根目录下的文件。 | 真实列目录（fs_list） | `exit=0;log~=fs_list` |
| 3 | 用 shell_run 运行 `grep -rn config src`，并总结命中的文件。 | 真实跑 Shell 且命中 | `exit=0;log~=grep;log~=config` |
| 4 | 读取 package.json，告诉我 test 脚本执行什么命令。 | 读到夹具的测试脚本 | `exit=0;log~=package.json` |
| 5 | 在 notes 目录下新建 hello.txt，写入一行：hello ignis。 | 文件被真实写出 | `exit=0;log~=fs_write;file=notes/hello.txt` |
| 6 | 把 notes/seed.txt 的内容改成只有一行：seed updated。 | 已有文件被真实改写 | `exit=0;log~=notes/seed.txt;file=notes/seed.txt` |
| 7 | 运行 npm test 并报告结果（不要修代码）。 | 真实跑测试；夹具预期失败但任务成功收尾 | `exit=0;log~=npm test` |
| 8 | 运行 node --version 并报告 Node 版本。 | 真实跑命令拿到版本 | `exit=0;log~=node` |
| 9 | 用 http_fetch 抓取 https://example.com 并给出页面标题。 | 真实 HTTP 抓取 | `exit=0;log~=http_fetch;log~=example.com` |
| 10 | 记住：这个项目的包管理器是 pnpm。 | 写入一条 active 事实 | `exit=0;fact~=pnpm` |
| 11 | 这个项目用什么包管理器？不要去跑工具，只用记忆回答。 | 新进程里检索到上一条记忆 | `exit=0;recall~=pnpm` |
| 12 | 用 server-health 技能检查本机健康状况：先 load_skill 读取技能全文，再按技能执行。 | 技能被选择并加载 | `exit=0;skill=server-health` |
| 13 | 用 project-analyzer 技能分析当前项目：先 load_skill 读取技能全文，再按技能执行。 | 第二个技能被选择并加载 | `exit=0;skill=project-analyzer` |
| 14 | 用 report-generator 技能生成一份简短的项目报告：先 load_skill 读取技能全文。 | 第三个技能被选择并加载 | `exit=0;skill=report-generator` |
| 15 | 用 notify_user 发一条 info 告警，标题：acceptance 任务提醒。 | 告警落库（可被 `ignis alerts` 看到） | `exit=0;log~=notify_user;alert>=1` |
| 16 | 创建一个调度任务：每隔 5 分钟运行 examples/test-server/check-health.sh --simulate 92，磁盘超过 85% 就用 notify_user 告警。 | 落一条 active 调度任务（不启动守护进程） | `exit=0;log~=schedule_create;job>=1` |
| 17 | 运行不存在的命令 definitely-not-a-command --version，解释发生了什么。 | 工具失败被如实报告，任务本身成功 | `exit=0;log~=definitely-not-a-command` |
| 18 | 先列出目录，再读取 package.json，最后总结这个项目是做什么的。 | 组合使用 fs_list 与 fs_read | `exit=0;log~=fs_list;log~=fs_read` |
| 19 | 尝试用 fs_read 读取 /etc/passwd，并把工具返回的结果原样告诉我。 | 被路径沙箱拒绝（日志含拒绝原因） | `exit=0;log~=路径越出工作区` |
| 20 | 用一句话解释 Ignis 是什么，不要调用任何工具。 | 纯文本回答且用量入账 | `exit=0;tokens>0` |

## 备注

- 第 7 条刻意跑一个预期失败的测试：验收的是「真的执行了工具、如实报告了失败」，不是「测试通过」。
- 第 19 条验收 `demo.md` 清单 #3 的拒绝侧：路径越出工作区时，工具必须被拒绝并把原因回给模型。
- 第 9 条需要机器能访问 `https://example.com`（harness 本就是联网工具）。
- 成功率门槛只统计 PASS 与 FAIL；环境性跳过（SKIP）不计入分母，但会逐条打印原因。
