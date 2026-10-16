# systemd 部署（用户服务）

用 systemd 用户单元常驻 Ignis 调度守护进程；日志走 journald。

## 一步：安装二进制

```sh
cargo install --path apps/cli        # 装进 ~/.cargo/bin/ignis
# 单元文件默认从 ~/.local/bin/ignis 启动；若不在那里，复制或软链过去：
mkdir -p ~/.local/bin && ln -sf ~/.cargo/bin/ignis ~/.local/bin/ignis
```

## 二步：安装单元文件并启动

```sh
mkdir -p ~/.config/systemd/user
cp deploy/ignis-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ignis-daemon
```

## 三步：确认与查看日志

```sh
systemctl --user status ignis-daemon
journalctl --user -u ignis-daemon -f
ignis daemon status                  # 与 pidfile 锁一致的进程状态
```

若不希望用户注销后守护进程被停掉，可开启 linger：`loginctl enable-linger "$USER"`。

守护进程的模型端点与密钥取 `~/.ignis/config.json`（或 `IGNIS_HOME` 指向目录里的配置）；也可在单元里用 `Environment=IGNIS_*` 覆盖（见 `ignis-daemon.service` 注释）。注意调度任务只跑 native 内核（见根目录 README 的「限制」）。

> **披露**：单元文件只做过语法检查（`systemd-analyze verify`，把 `ExecStart` 指到一个存在的可执行文件后通过），**未在本仓库的交付环境安装或启动过**（交付环境没有真实的 `~/.local/bin/ignis`，也没跑 `systemctl --user daemon-reload` / `enable --now`）；systemd 部分是文档级交付，以 [`ignis-daemon.service`](ignis-daemon.service) 为准——根目录 README 的 Docker 部分（镜像未在本环境构建）有同一性质的说明。
