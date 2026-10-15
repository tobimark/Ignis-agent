//! 敏感文件识别：命中即由上层按危险等级审批，不直接放行读取。

use std::path::Path;

/// 敏感文件清单（只看路径最后一段）：`.env`、`.env.*`、`*.pem`、`*.key`、`id_rsa*`、
/// `id_ed25519*`、`.netrc`、`.git-credentials`。
pub fn is_sensitive(rel_path: &Path) -> bool {
    let Some(name) = rel_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(name, ".env" | ".netrc" | ".git-credentials")
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn sensitive_file_table() {
        for name in [
            ".env",
            ".env.local",
            "a/b/.env.production",
            "/tmp/ws/.env",
            "cert.pem",
            "certs/server.key",
            "id_rsa",
            "id_rsa.pub",
            "sub/id_ed25519",
            "id_ed25519.pub",
            ".netrc",
            "home/.git-credentials",
        ] {
            assert!(is_sensitive(Path::new(name)), "{name}");
        }

        for name in [
            "env",
            ".envrc",
            "README.md",
            "key",
            "note.txt",
            "cert.pem.bak",
            "a.key.backup",
            "pem",
            "credentials",
            "src/main.rs",
        ] {
            assert!(!is_sensitive(Path::new(name)), "{name}");
        }
    }
}
