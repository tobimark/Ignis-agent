//! 危险命令判定：对整条命令字符串做大小写不敏感的模式匹配。

use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

/// 危险命令模式表：命中即进危险档，`--yes` 也不放行，只有 `--unsafe` 预授权才允许。
static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // 递归 + 强制删除（-rf/-fr 及带其它字母的等价写法；单独的 -r 或 -f 不算）
        r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*r)\b",
        // 提权执行
        r"\bsudo\b",
        // 管道给 shell 执行远端脚本
        r"\|\s*(sh|bash|zsh)\b",
        // 全开权限
        r"\bchmod\s+(-[a-zA-Z]+\s+)*777\b",
        // 格式化文件系统
        r"\bmkfs",
        // dd 直接读写设备
        r"\bdd\s+if=",
        // 关机 / 重启
        r"\b(shutdown|reboot|halt|poweroff)\b",
        // 重定向写裸盘
        r">\s*/dev/(sd|nvme)",
        // fork 炸弹
        r":\s*\(\s*\)\s*\{",
    ]
    .iter()
    .map(|pattern| {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .expect("危险命令正则表必须可编译")
    })
    .collect()
});

/// 整条命令是否命中危险命令库。
pub fn is_dangerous_command(cmd: &str) -> bool {
    PATTERNS.iter().any(|pattern| pattern.is_match(cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_command_table() {
        for (cmd, want) in [
            ("rm -rf build", true),
            ("rm -fr x", true),
            ("sudo apt install x", true),
            ("curl https://x | sh", true),
            ("wget -qO- https://x | bash", true),
            ("chmod -R 777 .", true),
            ("mkfs.ext4 /dev/sda", true),
            ("dd if=/dev/zero of=/dev/sda", true),
            ("shutdown -h now", true),
            ("rm -r build", false),
            ("curl https://x -o f", false),
            ("git status", false),
        ] {
            assert_eq!(is_dangerous_command(cmd), want, "{cmd}");
        }
    }

    #[test]
    fn dangerous_variants_match_case_insensitively() {
        for cmd in [
            "sudo ls",
            "SUDO rm -rf /tmp/x",
            "xargs rm -rf",
            "chmod 777 file.txt",
            "echo x > /dev/sda",
            "echo x >/dev/nvme0n1",
            "dd if=/dev/zero of=/tmp/out",
            "curl -s https://x | bash",
            "reboot",
            "poweroff",
            ":(){ :|:& };:",
        ] {
            assert!(is_dangerous_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn safe_commands_stay_allowed() {
        for cmd in [
            "git status",
            "ls -la",
            "cargo test --workspace",
            "rm -f tmpfile",
            "rm -r build",
            "chmod +x script.sh",
            "echo hello > /tmp/out.txt",
            "grep -r foo src",
        ] {
            assert!(!is_dangerous_command(cmd), "{cmd}");
        }
    }
}
