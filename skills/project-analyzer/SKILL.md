---
name: project-analyzer
description: Analyze a project's structure, run its tests, and locate failures.
---

# 项目分析

用于回答「这个项目是什么、测试怎么跑、为什么失败、怎么修」。**先拿证据再下结论**，每一步都要有工具输出支撑。

## 步骤

1. **看结构**：用 `fs_list` 列出工作区根目录（不传 `path` 即根），确认顶层目录与文件；可疑子目录再逐层列出。
2. **判断项目类型**：用 `fs_read` 读配置文件——`package.json` 是 Node/JavaScript、`pyproject.toml` 是 Python、`Cargo.toml` 是 Rust；一个都没有时按目录结构（`src/`、`tests/`、`Makefile` 等）推断，并说明依据。
3. **确定测试命令**：读 `package.json` 的 `scripts.test`、`pyproject.toml` 的测试配置或 `Cargo.toml` 的工作区布局，找到项目自带的测试命令（如 `npm test`、`pytest`、`cargo test`）；找不到就用项目现有脚本，不要臆造命令。
4. **跑测试**：用 `shell_run` 执行测试命令，记下退出码与失败摘要；失败很多时先聚焦第一个失败的测试文件或用例。
5. **定位原因**：用 `fs_read` 读失败测试与报错指向的源文件（长文件用 `offset`/`limit` 分段读），把报错信息、行号与代码逐条对上，给出根因（路径假设错误、缺依赖、断言过期、逻辑错误等），区分「已确认」与「待验证」。
6. **给修复方案**：说清「改哪个文件、改什么、为什么」。写操作（`fs_edit`/`fs_write`）要经过审批，所以动手前先说明意图并等确认。
7. **重跑验证**：改动后用同一条命令重跑测试，确认由失败变通过；仍有失败就回到第 5 步，不要在没验证的情况下宣称修好。
8. **汇总**：项目类型、使用的测试命令、根因、改动文件、重跑结果各一行；日志只留关键几行。
