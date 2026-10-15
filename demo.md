# Ignis Demo 开发方案 V0.1

产品定位：开源、自托管、具有长期记忆和主动执行能力的个人 AI Agent

我建议 Ignis 的第一个 Demo 不要做成简单的聊天机器人，也不要做成一个功能复杂的 Agent 平台。

应该做一个真正能运行、能完成任务、能记住用户、能在后台主动工作的最小个人 AI Agent。

我的建议是：用 2–3 周，做出一个具备 6 项核心能力、3 个完整演示场景的 MVP Demo。

IGNIS V0.1 · 产品目标

## 一个可以真正托付任务的个人 AI

# 6

项核心功能

# 3

个演示场景

# 2–3

周开发目标

按一名熟悉 Agent SDK 的开发者、复用现成运行时估算；不是开发工期保证。

## 一、Demo 最重要的是证明三个能力

Hermes Agent 目前已经覆盖记忆、Skills、定时任务、多平台消息和不同执行环境。Ignis 没必要在 V0.1 复制这整套功能。

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub

+1

我建议围绕三个核心闭环设计 Demo：

1\. 能执行：从自然语言到真实任务完成

不是只告诉用户怎么做，而是调用工具执行并交付结果。

2\. 能记忆：从一次任务到下一次任务

跨会话记住事实、偏好和成功经验，下次能复用。

3\. 能主动：从被动调用到后台持续工作

即使用户退出交互界面，任务也能按计划执行并反馈。

只要这三件事真的做通，Ignis 就已经具备一个个人自主 Agent 的基本产品形态。

## 二、Demo 应该实现哪些功能？

这是我建议的 V0.1 功能清单和实现程度。

| 模块          | 功能            | V0.1 要做到的程度           | 优先级 |
| ----------- | ------------- | --------------------- | --- |
| Agent Core  | 对话与工具调用       | 多轮任务、流式输出、真实工具执行      | P0  |
| Tool System | 文件、Shell、网络工具 | 最少 3 类工具，有权限控制        | P0  |
| Memory      | 长期记忆          | 跨会话持久化、检索、修改、删除       | P0  |
| Skills      | 技能加载与执行       | 自动选择并运行 2–3 个本地 Skill | P0  |
| Scheduler   | 后台定时任务        | 创建、触发、暂停、查看执行记录       | P0  |
| Gateway     | CLI + 一个消息入口  | 共享任务、记忆和执行状态          | P1  |

### 1. Agent Core：实现到什么程度？

用户执行：

```
ignis
```

出现交互式 CLI，例如：

IGNIS v0.1

● Ready

Workspace: \~/projects/demo

Runtime: Codex · Skills: 3 · Memory: enabled

❯ 帮我分析这个项目，找到测试失败的原因。

● Running task...

→ Reading package.json

→ Running tests

→ Inspecting test failure

→ Preparing proposed fix

发现 1 个失败测试，原因是配置路径不正确。已生成修改方案，等待你确认应用。

[Approve]  [Reject]  [Details]

CLI 概念展示，并非已经实现的 Ignis 界面。

V0.1 的完成标准： Agent 能调用工具、根据工具结果继续推理、交付结果，并把执行过程写入日志。支持至少一种权限审批和主动中止。

建议第一版直接复用现有 Agent SDK。例如 Codex SDK 已支持会话及结构化流式事件，适合作为初始执行后端。

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub

### 2. Memory：只做真正有价值的记忆

建议划分为两类：

| 类型   | 内容             | 示例                         |
| ---- | -------------- | -------------------------- |
| 用户记忆 | 长期偏好、环境信息、项目背景 | 用户偏好 Python；某项目主要使用 Ubuntu |
| 任务经验 | 成功执行过的步骤、常见错误  | 某项目构建前要安装特定依赖              |

最小实现可以采用 SQLite + Markdown，不必一开始引入向量数据库。

做到可以：

- 记住和查询指定事实。
- 在新会话中自动使用相关记忆。
- 允许用户查看、修改和删除。
- 用户纠正错误信息后，不再继续使用旧信息。

验收重点：重启 Ignis 后记忆仍然存在，而且确实影响下一次任务的执行。

### 3. Skills：复用而不是开发复杂技能平台

直接采用兼容 `SKILL.md` 的目录约定。例如：

```
~/.ignis/skills/
├── project-analyzer/
│   └── SKILL.md
├── server-health/
│   └── SKILL.md
└── report-generator/
    └── SKILL.md
```

V0.1 完成这四件事就够了：

1. 扫描本地 Skills。
2. 根据任务选择相关 Skill。
3. 读取 Skill 指令并使用工具执行。
4. 返回执行结果和实际使用的 Skill 名称。

不需要开发 Skill Marketplace，也不需要自动创造上百个 Skills。

### 4. Scheduler：Demo 最能体现自主性的模块

用户可以输入：

> 每天早上 9 点检查测试服务器的磁盘、内存和 Docker 状态。有异常就通知我。

Ignis 应当可以创建任务，并在后台执行。

第一版只要支持间隔任务和常用 Cron 表达式，能够查看执行历史、暂停、恢复即可。

关键：关闭 CLI 后，调度服务仍然运行。 任务失败有记录，不无限重试，也不能绕过正常的工具权限限制。

### 5. Gateway：只支持两个入口

第一版建议：

- CLI：开发者使用和调试。
- Telegram：远程交互和任务结果通知。

这两个入口应该共享同一份记忆、任务数据库和权限体系。

不需要同时接入微信、Discord、Slack、WhatsApp。一个真正可用的远程入口，比五个只能聊天的入口更有价值。

## 三、最关键的三个演示场景

这是我认为 Demo 的核心。功能能否工作，不应只靠测试接口，而应该用完整的用户故事来验证。

[Picsart tutorials — Learn to create with AI | Picsart](https://images.openai.com/static-rsc-4/wRnJGitrVjTbF9JZmIdSLgl1NN-gb9x5f7-mtwkvChMjAuqr3EVvu5WZ9oh74H7oZKY3i95MxDq-aPDhSHU-T09gO6QH7IpRKiROj7Kwyns6EoJe6q2u4FFxjnAnqehekIgIsiUZhMvAeTqY5jBJwTCzVSMWlSgbwwGSchbPCBg?purpose=inline)

Demo A · AI 真正完成工作

P0 · 必须成功

用户说：“分析这个项目的测试失败原因，修复它并重新运行测试。”

Ignis 读取文件、运行测试、定位问题、请求写入许可、修改文件、重新测试并汇报结果。

证明： Ignis 不只是会聊天，而是真的能执行任务。

[GitHub Copilot: Claude Fable 5 is GA | 24 AI](https://images.openai.com/static-rsc-4/vEcyC9S5UJOVYGLz2H-8RWpdziQXCXoO-OqmxdbR2hMB8v5wi6x1_2IRVT7KgDVdegZJtUTLF-6p5NFenf7mI7ZF_loZynWcrq9UgNs_TIV3IRF4nZutmHqwN93x0QfGAueXcxayYqRU0ExTEjBB2brnIpEfR7s_TPCL8CXKvHo?purpose=inline)

Demo B · AI 记住过去的经验

P0 · 必须成功

用户第一次告诉 Ignis：“这个项目使用 uv 管理 Python 环境。”

结束会话，重启 Ignis，再说：“帮我检查这个项目的依赖并运行测试。”

Ignis 能检索并应用此前记忆，优先使用 uv 相关工作流，而非未经确认地改用 pip。

证明： Ignis 具备跨会话的持久记忆。

[Daniel LaForce | Systems Architect & Infrastructure Engineer | Daniel LaForce | LaForceIT](https://images.openai.com/static-rsc-4/uYOTTaUThygR8ZPk1rs22SEfAObwg-aNZWGUpG1UczqIf5fsAncBmcEfikXynpS38ZwAncD5k_u-05PP2YrU9MuMVfgQf9i61ZedM4noOHUDEBaXEJCiuunK6xUFKprrJnt5InDK7uB6V4s-y_QsL3ks2_nY5M44zL3XOv7zOlc?purpose=inline)

Demo C · AI 在后台主动工作

P0 · 必须成功

用户说：“每隔 10 分钟检查测试服务器的磁盘使用率，超过 85% 就通知我。”

Ignis 创建计划任务。用户退出 CLI，后台服务继续检查。

当测试环境模拟出阈值超限时，Ignis 将告警发送至 Telegram，并记录执行结果。

证明： Ignis 不依赖用户一直发消息才能工作。

我最看重 Demo C。它能让人直观理解个人 AI Agent 与普通聊天工具的区别。

## 四、技术架构：尽量简单，但保留未来扩展性

IGNIS

Personal Autonomous Agent

CLI

Telegram

Ignis Core

Session & Task

Memory

Skills

Scheduler

Approval & Execution Logs

Agent Runtime Adapter

Codex SDK · 首版仅实现一个后端

Filesystem

Shell

HTTP / Web

SQLite + 本地持久化

### 推荐技术栈

| 组件      | 推荐技术                      | 理由                     |
| ------- | ------------------------- | ---------------------- |
| 主语言     | TypeScript / Node.js      | 方便集成 Agent SDK、CLI、Bot |
| Runtime | Codex SDK                 | 首版避免重写执行循环             |
| CLI     | Commander + Ink（或简单 REPL） | 快速搭建交互界面               |
| 服务层     | Fastify                   | 轻量，适合本地 API            |
| 数据库     | SQLite                    | 简单、自托管                 |
| 记忆      | Markdown + SQLite FTS5    | 易检查，支持全文检索             |
| 定时任务    | Cron 调度器 + 持久化任务表         | 适合单机 Demo              |
| 消息入口    | Telegram Bot API          | 容易建立完整演示               |
| 部署      | Docker / Linux systemd    | 后台常驻、易迁移               |

一个很重要的工程原则：Ignis Core 负责长期状态、权限、任务和记忆；Agent Runtime 负责推理、工具使用与执行循环。

Runtime Adapter 在 V0.1 只需定义接口并实现一种后端。不要为了未来可能更换执行后端，就提前开发完整的多 Runtime 系统。

示意目录：

```
ignis/
├── apps/
│   ├── cli/
│   └── gateway/
├── packages/
│   ├── core/
│   │   ├── tasks/
│   │   ├── sessions/
│   │   ├── memory/
│   │   ├── skills/
│   │   ├── scheduler/
│   │   └── security/
│   ├── runtime/
│   │   ├── interface.ts
│   │   └── codex.ts
│   └── storage/
├── skills/
│   ├── project-analyzer/
│   ├── server-health/
│   └── report-generator/
├── examples/
├── tests/
└── README.md
```

## 五、哪些功能绝对不要在 Demo 阶段做？

我建议明确推迟下面这些内容。

| 功能               | V0.1 决策 | 原因              |
| ---------------- | ------- | --------------- |
| 多 Agent 协作       | 暂不做     | 单 Agent 还没证明稳定性 |
| 完整 Web Dashboard | 暂不做     | CLI 已足够验证核心能力   |
| 语音实时交互           | 暂不做     | 对任务执行闭环帮助有限     |
| 自动编写并执行新 Skills  | 暂不做     | 需要技能验证和安全机制     |
| 多模型路由与负载均衡       | 暂不做     | 首先证明一个后端可用      |
| 向量数据库与知识图谱       | 暂不做     | 简单记忆先满足需求       |
| 多用户 / SaaS       | 暂不做     | 首版定位单用户自托管      |
| Skill 商店         | 暂不做     | 直接加载本地 Skill 即可 |

但我不建议推迟基础安全功能。

V0.1 至少要做到：Shell 和文件访问限制在指定工作空间；危险命令、敏感文件读取和对外操作经过审批；Telegram 绑定授权用户；Token 不写入日志；后台任务设置执行时间与资源上限。

对于 Demo，先把执行限制在专门的测试工作空间和测试服务器中，不要直接让 Agent 拥有宿主机无限制权限。

## 六、开发计划与完成度

第 1 周

基础能力

让 Ignis 真实执行任务

- CLI、Agent Runtime、工具调用
- 结构化任务日志、权限审批
- SQLite 会话与任务持久化
- 完成 Demo A

第 2 周

记忆与技能

让 Ignis 具备持续性

- 长期记忆与检索
- SKILL.md 加载与选择
- 后台调度服务
- 完成 Demo B

第 3 周

产品闭环

让 Ignis 能被真正演示

- Telegram 接入
- 后台任务通知与历史查看
- Docker / systemd 部署
- 完成 Demo C、异常恢复测试和 README

这里的 2–3 周默认是单开发者、有经验、可正常调用模型 API 的情况。若需要自己实现 Agent 执行循环、深度 SSH 隔离或企业级安全，时间会明显增加。

## 七、Demo 的验收标准

我建议用下面这个检查表作为 V0.1 发布门槛。

V0.1 验收清单

0/10

- [ ] 能在干净的 Ubuntu 环境按文档安装并启动
- [ ] 真实完成一个文件/代码任务，而不是返回模拟结果
- [ ] 写入等高风险操作必须经过正确的权限检查
- [ ] 关闭并重启程序后，能读取之前保存的记忆
- [ ] 至少 2 个 Skill 能被正确发现、选择和执行
- [ ] CLI 退出后，后台计划任务仍能触发
- [ ] Telegram 能收到真实任务结果或告警
- [ ] 用户能查看任务结果、失败原因和执行日志
- [ ] 服务重启后，任务记录不丢失、不重复执行危险操作
- [ ] 任务具备超时、取消和调用成本上限

对于稳定性，建议再设三个量化目标：在固定测试环境中，三个演示场景分别连续成功 3 次；20 次常见任务测试的端到端成功率达到 80% 以上；10 次服务重启测试不出现任务重复执行或记录丢失。这些是建议的内部 Demo 门槛，不代表生产可用标准。

## 八、我对 Ignis V0.1 最终的建议

Demo 的最终交付物不应该是一段展示视频或者一套 UI，而应该是一个真正可下载、可运行、可验证的开源仓库。

交付内容包括：

- 一个可安装运行的 `ignis` CLI。
- 一个后台 Gateway 服务。
- Memory、Skills、Scheduler 三个核心模块。
- 至少三个开箱可用的 Skills。
- 一个 Telegram Bot 接入示例。
- 三个端到端演示和自动化测试。
- 一份可以让陌生开发者完成安装的 README。

在产品层面，我还建议给 Ignis 一个特别清晰的设计理念：

## Ignis should remember, act, and improve.

Ignis 应该能够记忆、执行，并在用户可控的前提下，从经验中持续改进。

V0.1 不必实现真正的自主学习。只需要先做到保存并复用成功经验；自动生成、评估和改进 Skills 可以留到 V0.2。

最重要的一点：V0.1 应该优先证明 Ignis 是一个可托付真实任务的个人 Agent，而不是证明我们能够开发多少 Agent 基础设施。

如果按照这个边界完成 Demo，下一版再加入更丰富的个人记忆、能力扩展和运行时选择，就有一条相当清晰的产品演进路径。
