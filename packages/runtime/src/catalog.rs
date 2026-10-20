//! 通用模型能力目录：从 models.dev 的公开目录提取 `provider/model → limit.context/output`。
//!
//! 这一层解决「Provider API 不返回上下文窗口」的模型（例如直接调用 MiniMax 的
//! `/v1/models` 只有 id/object/created/owned_by）。目录是数据，不是按模型写的 Rust 常量：
//! 首次使用拉取 `https://models.dev/api.json`，压缩后缓存到 `~/.ignis/model_catalog.json`
//! （24h TTL）；离线时用缓存或内置 Registry 兜底。
//!
//! 查找顺序：base_url 的 host 先匹配 Provider（`api.minimax.cn` → `minimax`），再按
//! `provider/model` 精确匹配；随后尝试 canonical id 与全目录 model id。多 Provider 同名模型
//! 值不一致时返回 `None`（不猜），交给上层显示 Unknown 或用户覆盖。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// models.dev 的公开目录。
const CATALOG_URL: &str = "https://models.dev/api.json";
/// 缓存有效期：一天刷新一次即可；手动删除缓存文件可强制刷新。
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 目录拉取超时：比单模型探测长一点（目录是几百 KB 的 JSON）。
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// base_url host → models.dev provider id 的显式别名。
///
/// 通用规则是「host 里包含 provider id 就用它」；别名只补那些 host 与 provider id
/// 不同名的常见服务（Gemini、MiniMax、本地推理服务）。
const PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("openrouter.ai", "openrouter"),
    ("api.anthropic.com", "anthropic"),
    ("api.openai.com", "openai"),
    ("generativelanguage.googleapis.com", "google"),
    ("api.minimax.", "minimax"),
    ("api.deepseek.com", "deepseek"),
    ("api.mistral.ai", "mistral"),
    ("api.groq.com", "groq"),
    ("api.x.ai", "xai"),
    ("localhost:11434", "ollama"),
    ("127.0.0.1:11434", "ollama"),
    ("localhost:1234", "lmstudio"),
    ("127.0.0.1:1234", "lmstudio"),
    ("vllm", "vllm"),
];

/// 目录里的一条模型能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// models.dev 的 Provider id（`minimax` / `openrouter` / `openai`…）。
    pub provider_id: String,
    /// 目录里的模型 id（保留原始大小写）。
    pub model_id: String,
    /// 上下文窗口（tokens）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 最大输入 tokens（目录给出 `limit.input` 时才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// 最大输出 tokens（目录给出 `limit.output` 时才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// 跨 Provider 的 canonical id（例如 `minimax/MiniMax-M3`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
}

/// 通用模型目录：原始条目 + 三张查找表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalog {
    entries: Vec<CatalogEntry>,
    by_provider_model: HashMap<(String, String), usize>,
    by_model: HashMap<String, Vec<usize>>,
    by_canonical: HashMap<String, usize>,
    provider_ids: Vec<String>,
}

impl ModelCatalog {
    /// 空目录（测试或离线兜底用）。
    pub fn empty() -> Self {
        Self::from_entries(Vec::new())
    }

    /// 从条目集合建目录（测试与缓存加载共用）。
    pub fn from_entries(entries: Vec<CatalogEntry>) -> Self {
        let entries: Vec<CatalogEntry> = entries
            .into_iter()
            .filter(|entry| {
                !entry.provider_id.trim().is_empty() && !entry.model_id.trim().is_empty()
            })
            .collect();
        let mut by_provider_model = HashMap::new();
        let mut by_model: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_canonical = HashMap::new();
        let mut provider_ids = Vec::new();
        let mut seen_providers = HashSet::new();

        for (index, entry) in entries.iter().enumerate() {
            let provider = normalize(&entry.provider_id);
            let model = normalize(&entry.model_id);
            by_provider_model.insert((provider.clone(), model.clone()), index);
            by_model.entry(model).or_default().push(index);
            if let Some(canonical) = &entry.canonical_id {
                by_canonical.entry(normalize(canonical)).or_insert(index);
            }
            if seen_providers.insert(provider.clone()) {
                provider_ids.push(provider);
            }
        }
        // 长 id 优先，避免短 id 在 host 匹配里抢走更具体的 Provider。
        provider_ids.sort_by_key(|provider| std::cmp::Reverse(provider.len()));

        Self {
            entries,
            by_provider_model,
            by_model,
            by_canonical,
            provider_ids,
        }
    }

    /// 解析 models.dev 的完整 JSON（顶层是 provider id → provider 对象）。
    pub fn from_models_dev_json(bytes: &[u8]) -> Option<Self> {
        let root: Value = serde_json::from_slice(bytes).ok()?;
        let providers = root.as_object()?;
        let mut entries = Vec::new();
        for (provider_id, provider) in providers {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            for (model_id, model) in models {
                let limit = model.get("limit");
                let limit_value = |key: &str| {
                    limit
                        .and_then(|limit| limit.get(key))
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0)
                };
                let context_window = limit_value("context").or_else(|| {
                    model
                        .get("context_length")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0)
                });
                let max_input_tokens = limit_value("input");
                let max_output_tokens = limit_value("output").or_else(|| {
                    model
                        .get("max_output_tokens")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0)
                });
                if context_window.is_none()
                    && max_input_tokens.is_none()
                    && max_output_tokens.is_none()
                {
                    continue;
                }
                entries.push(CatalogEntry {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    context_window,
                    max_input_tokens,
                    max_output_tokens,
                    canonical_id: model
                        .get("canonical_model_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        Some(Self::from_entries(entries))
    }

    /// 查找模型：Provider 精确匹配 → canonical id → 全目录同名模型（值一致才算命中）。
    pub fn lookup(&self, base_url: &str, model: &str) -> Option<&CatalogEntry> {
        let model_key = normalize(model);
        let provider = self.provider_hint(base_url);

        if let Some(provider) = provider
            && let Some(&index) = self
                .by_provider_model
                .get(&(normalize(provider), model_key.clone()))
        {
            return self.entries.get(index);
        }
        if let Some(&index) = self.by_canonical.get(&model_key) {
            return self.entries.get(index);
        }
        let indices = self.by_model.get(&model_key)?;
        let first = self.entries.get(*indices.first()?)?;
        if let Some(provider) = provider
            && let Some(entry) = indices
                .iter()
                .filter_map(|&index| self.entries.get(index))
                .find(|entry| entry.provider_id.eq_ignore_ascii_case(provider))
        {
            return Some(entry);
        }
        // 多 Provider 同名模型：所有候选的能力值一致才返回，否则宁可不猜。
        if indices.iter().all(|&index| {
            let entry = &self.entries[index];
            entry.context_window == first.context_window
                && entry.max_input_tokens == first.max_input_tokens
                && entry.max_output_tokens == first.max_output_tokens
        }) {
            Some(first)
        } else {
            None
        }
    }

    /// base_url 的 host 对应的 Provider id（`api.minimax.cn` → `minimax`）。
    pub fn provider_hint(&self, base_url: &str) -> Option<&str> {
        let host = reqwest::Url::parse(base_url)
            .ok()?
            .host_str()?
            .to_ascii_lowercase();
        for &(needle, provider) in PROVIDER_ALIASES {
            if host.contains(needle) {
                return Some(provider);
            }
        }
        self.provider_ids
            .iter()
            .find(|provider| host.contains(provider.as_str()))
            .map(String::as_str)
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空目录。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 读取缓存：返回目录与 `fetched_at`（秒）；文件缺失/损坏返回 `None`。
    pub fn load_cache(path: &Path) -> Option<(Self, u64)> {
        let bytes = fs::read(path).ok()?;
        let snapshot: CatalogSnapshot = serde_json::from_slice(&bytes).ok()?;
        Some((Self::from_entries(snapshot.entries), snapshot.fetched_at))
    }

    /// 原子写缓存（同目录临时文件 + rename，unix 下落 0600）。
    pub fn write_cache(path: &Path, catalog: &Self, fetched_at: u64) -> bool {
        let snapshot = CatalogSnapshot {
            fetched_at,
            entries: catalog.entries.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&snapshot) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, bytes).is_err() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&temporary, path).is_ok()
    }

    /// 读缓存；过期则拉取 models.dev，失败时回退到过期缓存。
    pub async fn load_or_fetch(http: &reqwest::Client, path: &Path) -> Option<Self> {
        let now = unix_now();
        let cached = Self::load_cache(path);
        if let Some((catalog, fetched_at)) = &cached
            && now.saturating_sub(*fetched_at) <= CACHE_TTL.as_secs()
        {
            return Some(catalog.clone());
        }

        let response = http.get(CATALOG_URL).timeout(FETCH_TIMEOUT).send().await;
        if let Ok(response) = response
            && let Ok(response) = response.error_for_status()
            && let Ok(bytes) = response.bytes().await
            && let Some(catalog) = Self::from_models_dev_json(&bytes)
            && !catalog.is_empty()
        {
            let _ = Self::write_cache(path, &catalog, now);
            return Some(catalog);
        }

        cached.map(|(catalog, _)| catalog)
    }
}

/// 可缓存的目录快照（只保留能力字段，不落盘整份 models.dev JSON）。
#[derive(Debug, Serialize, Deserialize)]
struct CatalogSnapshot {
    fetched_at: u64,
    entries: Vec<CatalogEntry>,
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// 当前 Unix 秒（缓存时间戳用；时钟倒退时按 0 处理）。
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 测试夹具：小型 models.dev 形状目录（MiniMax / OpenRouter / proxy / OpenAI）。
#[cfg(test)]
pub(crate) const FIXTURE: &str = r#"{
  "minimax": {
    "id": "minimax",
    "api": "https://api.minimax.io/anthropic/v1",
    "models": {
      "MiniMax-M3": {
        "id": "MiniMax-M3",
        "limit": { "context": 1000000, "output": 512000 },
        "canonical_model_id": "minimax/MiniMax-M3"
      },
      "MiniMax-M2": {
        "id": "MiniMax-M2",
        "limit": { "context": 204800, "output": 131072 }
      }
    }
  },
  "openrouter": {
    "id": "openrouter",
    "api": "https://openrouter.ai/api/v1",
    "models": {
      "minimax/minimax-m3": {
        "id": "minimax/minimax-m3",
        "limit": { "context": 1048576, "output": 512000 }
      }
    }
  },
  "proxy": {
    "id": "proxy",
    "api": "https://proxy.example.com/v1",
    "models": {
      "MiniMax-M3": {
        "id": "MiniMax-M3",
        "limit": { "context": 500000, "output": 100000 }
      }
    }
  },
  "openai": {
    "id": "openai",
    "api": "https://api.openai.com/v1",
    "models": {
      "gpt-4o-mini": {
        "id": "gpt-4o-mini",
        "limit": { "context": 200000, "input": 190000, "output": 100000 }
      }
    }
  }
}"#;

#[cfg(test)]
pub(crate) fn fixture_catalog() -> ModelCatalog {
    ModelCatalog::from_models_dev_json(FIXTURE.as_bytes()).expect("fixture 必须是合法 JSON")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ModelCatalog, fixture_catalog, unix_now};

    #[test]
    fn minimax_m3_resolves_through_the_provider_hint() {
        let catalog = fixture_catalog();
        assert_eq!(
            catalog.provider_hint("https://api.minimax.cn/v1"),
            Some("minimax")
        );
        let entry = catalog
            .lookup("https://api.minimax.cn/v1", "MiniMax-M3")
            .expect("MiniMax-M3 应命中 minimax 条目");
        assert_eq!(entry.provider_id, "minimax");
        assert_eq!(entry.context_window, Some(1_000_000));
        assert_eq!(entry.max_output_tokens, Some(512_000));
        assert_eq!(entry.canonical_id.as_deref(), Some("minimax/MiniMax-M3"));
    }

    #[test]
    fn provider_specific_entries_do_not_cross_contaminate() {
        let catalog = fixture_catalog();
        let minimax = catalog
            .lookup("https://api.minimax.cn/v1", "MiniMax-M3")
            .expect("minimax");
        let openrouter = catalog
            .lookup("https://openrouter.ai/api/v1", "minimax/minimax-m3")
            .expect("openrouter");
        assert_eq!(minimax.context_window, Some(1_000_000));
        assert_eq!(openrouter.context_window, Some(1_048_576));
        assert_ne!(minimax.context_window, openrouter.context_window);
    }

    #[test]
    fn generic_provider_ids_match_hosts_and_canonical_ids_work() {
        let catalog = fixture_catalog();
        assert_eq!(
            catalog.provider_hint("https://proxy.example.com/v1"),
            Some("proxy")
        );
        assert_eq!(
            catalog
                .lookup("https://proxy.example.com/v1", "MiniMax-M3")
                .and_then(|entry| entry.context_window),
            Some(500_000)
        );
        // canonical id 可以直接查，不依赖 host。
        assert_eq!(
            catalog
                .lookup("https://unknown.example.com/v1", "minimax/MiniMax-M3")
                .and_then(|entry| entry.context_window),
            Some(1_000_000)
        );
    }

    #[test]
    fn ambiguous_model_ids_are_not_guessed() {
        let catalog = fixture_catalog();
        // minimax 1M 与 proxy 500K 同名；未知 host 无法判定 Provider。
        assert_eq!(
            catalog.lookup("https://unknown.example.com/v1", "MiniMax-M3"),
            None
        );
        assert_eq!(
            catalog.lookup("https://api.minimax.cn/v1", "not-a-model"),
            None
        );
    }

    #[test]
    fn catalog_keeps_input_and_output_limits_separate() {
        let catalog = fixture_catalog();
        let entry = catalog
            .lookup("https://api.openai.com/v1", "gpt-4o-mini")
            .expect("openai gpt-4o-mini");
        assert_eq!(entry.context_window, Some(200_000));
        assert_eq!(entry.max_input_tokens, Some(190_000));
        assert_eq!(entry.max_output_tokens, Some(100_000));
    }

    #[test]
    fn cache_roundtrip_keeps_the_catalog() {
        let path = std::env::temp_dir().join(format!(
            "ignis-model-catalog-test-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let catalog = fixture_catalog();
        let fetched_at = unix_now();
        assert!(ModelCatalog::write_cache(&path, &catalog, fetched_at));
        let (loaded, loaded_at) = ModelCatalog::load_cache(&path).expect("缓存应可读");
        assert_eq!(loaded_at, fetched_at);
        assert_eq!(
            loaded
                .lookup("https://api.minimax.cn/v1", "MiniMax-M3")
                .and_then(|entry| entry.context_window),
            Some(1_000_000)
        );
        let _ = std::fs::remove_file(path);
    }
}
