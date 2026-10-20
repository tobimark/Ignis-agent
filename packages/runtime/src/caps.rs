//! 模型能力解析：上下文窗口、输入/输出上限与来源。
//!
//! 解析优先级（设计 §4）：Runtime API → Provider API → 通用模型目录（models.dev）→ 本地
//! Registry → 用户配置 → Unknown。网络探测只读公开/本地的能力端点，1.2s 超时；模型目录
//! 独立缓存（24h TTL），失败时回退到缓存/内置 Registry，不阻塞任务执行。
//!
//! **上下文窗口 ≠ 最大输出**：两者是独立字段；[`ModelCapabilities::effective_limit`] 是上下文
//! 总窗口，最大输出只展示（并在 `/model info` 里单列），不从窗口里偷偷扣减。
//!
//! Registry 是保守的离线兜底：只收录来源可追溯、值稳定的常见模型；Ollama / vLLM 的窗口随运行时
//! 配置变化，因此不进入 Registry，只信 Runtime API 或用户显式配置。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::ModelConfig;
use crate::catalog::ModelCatalog;

/// 单次网络探测的超时：启动/切换模型时最多等这么久，失败回退静态解析。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1200);

/// 能力信息的来源（`/model info` 的 `Source` 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// 运行时的实际限制（vLLM `max_model_len` / Ollama `num_ctx` / `context_length`）。
    Runtime,
    /// Provider 的模型元数据 API（OpenRouter `/models`）。
    ProviderApi,
    /// 通用模型目录（models.dev，缓存到 `~/.ignis/model_catalog.json`）。
    Catalog,
    /// 本地模型能力 Registry（离线兜底）。
    Registry,
    /// 用户显式配置（`model.context_window` / `model.max_output_tokens`）。
    UserConfig,
    /// 无法可靠确定：UI 必须显示 `?` / `--` / `Unknown`，不得猜测。
    Unknown,
}

impl CapabilitySource {
    /// 展示用英文标签（CLI chrome）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Runtime => "Runtime API",
            Self::ProviderApi => "Provider API",
            Self::Catalog => "Model Catalog",
            Self::Registry => "Model Registry",
            Self::UserConfig => "User config",
            Self::Unknown => "Unknown",
        }
    }
}

/// 解析出的模型能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Provider 展示名（`OpenAI` / `OpenRouter` / `Ollama` / `vLLM` / `OpenAI-compatible`…）。
    pub provider: String,
    /// 实际调用的模型标识（保留用户配置的原始大小写）。
    pub model_id: String,
    /// 模型原生上下文窗口（Provider API / Registry）。
    pub context_window: Option<u64>,
    /// 最大输入 tokens（只有 Provider 明确给出时才有值）。
    pub max_input_tokens: Option<u64>,
    /// 最大输出 tokens（Provider API / Registry / 用户配置）。
    pub max_output_tokens: Option<u64>,
    /// 运行时实际限制（vLLM / Ollama）。
    pub runtime_limit: Option<u64>,
    /// 用户显式配置的上下文上限。
    pub user_limit: Option<u64>,
    /// 有效上下文窗口 = min(context_window, runtime_limit, user_limit)。
    pub effective_limit: Option<u64>,
    /// [`ModelCapabilities::context_window`] 的来源（没有原生窗口时是有效值的来源）。
    pub source: CapabilitySource,
    /// 有效限制的来源（被用户配置压低时是 `UserConfig`）。
    pub effective_source: CapabilitySource,
}

impl ModelCapabilities {
    /// 一个「什么都不知道」的能力集合：只保留 provider/model 展示名。
    pub fn unknown(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            runtime_limit: None,
            user_limit: None,
            effective_limit: None,
            source: CapabilitySource::Unknown,
            effective_source: CapabilitySource::Unknown,
        }
    }

    /// 只走本地信息（Registry + 用户配置）的同步解析；启动首帧与 API 不可达时使用。
    pub fn static_resolve(cfg: &ModelConfig) -> Self {
        Self::static_resolve_with_catalog(cfg, None)
    }

    /// 带通用模型目录的同步解析：目录值优先于内置 Registry，用户配置仍是上限。
    pub fn static_resolve_with_catalog(cfg: &ModelConfig, catalog: Option<&ModelCatalog>) -> Self {
        let kind = ProviderKind::detect(cfg);
        let mut caps = Self::unknown(kind.label(), cfg.model.clone());
        let user_limit = cfg.context_window.filter(|value| *value > 0);
        caps.user_limit = user_limit;

        // 1. 通用目录（models.dev）：跨 Provider 覆盖最广，先于内置 Registry。
        if let Some(entry) = catalog.and_then(|catalog| catalog.lookup(&cfg.base_url, &cfg.model)) {
            if entry.context_window.is_some() {
                caps.context_window = entry.context_window;
                caps.source = CapabilitySource::Catalog;
            }
            caps.max_input_tokens = entry.max_input_tokens;
            caps.max_output_tokens = entry.max_output_tokens;
        }

        // 2. 内置 Registry：目录没收录/离线时的兜底；只补缺失字段。
        if kind.uses_registry()
            && let Some(entry) = registry_lookup(&cfg.model)
        {
            if caps.context_window.is_none() {
                caps.context_window = Some(entry.context_window);
                caps.source = CapabilitySource::Registry;
            }
            if caps.max_output_tokens.is_none() {
                caps.max_output_tokens = entry.max_output_tokens;
            }
        }

        // 3. 用户显式配置的最大输出优先于目录/Registry（用户最了解自建服务）。
        if let Some(max_output) = cfg.max_output_tokens.filter(|value| *value > 0) {
            caps.max_output_tokens = Some(max_output);
        }

        // 4. 没有原生窗口、只有用户配置：把用户值当作「声明的窗口」，来源标为用户配置。
        if caps.context_window.is_none()
            && let Some(user_limit) = user_limit
        {
            caps.context_window = Some(user_limit);
            caps.source = CapabilitySource::UserConfig;
        }

        recompute_effective(&mut caps);
        caps
    }

    /// 有效窗口的展示词：`?` 表示未知。
    pub fn window_word(&self) -> String {
        match self.effective_limit {
            Some(limit) => limit.to_string(),
            None => "?".to_string(),
        }
    }
}

/// Provider 类型：只用于选择探测端点与展示名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    OpenRouter,
    Anthropic,
    Gemini,
    Ollama,
    Vllm,
    /// 本机自定义 OpenAI-compatible 端点：尝试 vLLM 形状的 `/models`，失败则回退 Registry。
    Local,
    /// 远端自定义 OpenAI-compatible 端点。
    Custom,
}

impl ProviderKind {
    /// 从 `base_url` 探测 Provider。只做保守的字符串/URL 判断，不发起网络请求。
    pub fn detect(cfg: &ModelConfig) -> Self {
        let url = cfg.base_url.to_ascii_lowercase();
        if url.contains("openrouter.ai") {
            Self::OpenRouter
        } else if url.contains("api.anthropic.com") {
            Self::Anthropic
        } else if url.contains("generativelanguage.googleapis.com") {
            Self::Gemini
        } else if url.contains("api.openai.com") {
            Self::OpenAi
        } else if url.contains(":11434") || url.contains("ollama") {
            Self::Ollama
        } else if url.contains("vllm") {
            Self::Vllm
        } else if is_local_url(&url) {
            Self::Local
        } else {
            Self::Custom
        }
    }

    /// Provider 展示名（写进 [`ModelCapabilities::provider`]）。
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Google Gemini",
            Self::Ollama => "Ollama",
            Self::Vllm => "vLLM",
            Self::Local => "OpenAI-compatible (local)",
            Self::Custom => "OpenAI-compatible",
        }
    }

    /// 是否允许用本地 Registry 作为窗口回退。
    ///
    /// Ollama / vLLM 的窗口由运行时配置决定（同一个模型名可以是 4K 也可以是 128K），
    /// 因此只信 Runtime API 或用户配置；其余 Provider 的模型名与发布窗口一一对应。
    fn uses_registry(self) -> bool {
        !matches!(self, Self::Ollama | Self::Vllm)
    }

    /// 是否需要探测 `/models`（vLLM 形状）。
    fn probes_models(self) -> bool {
        matches!(self, Self::OpenRouter | Self::Vllm | Self::Local)
    }
}

/// 本机端点判断：host 是回环/`0.0.0.0`/`.local`。
fn is_local_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return url.contains("localhost") || url.contains("127.0.0.1");
    };
    match parsed.host_str() {
        Some("localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]") => true,
        Some(host) => host.ends_with(".local"),
        None => false,
    }
}

/// Registry 的一条：前缀匹配 + 发布值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryEntry {
    /// 模型名前缀（小写）。
    pub prefix: &'static str,
    /// 发布上下文窗口（tokens）。
    pub context_window: u64,
    /// 发布最大输出（tokens）；未知为 `None`。
    pub max_output_tokens: Option<u64>,
}

/// 保守的本地 Registry：只收录来源可追溯、值稳定的常见模型。
///
/// 匹配用最长前缀，因此 `gpt-4o-mini-2024-07-18` 会命中 `gpt-4o-mini`，而 `gpt-4o` 不会误吞
/// `gpt-4o-mini`。OpenRouter 的 `provider/model` 会先取最后一段再匹配。
/// 新增条目前先确认官方文档的窗口值；不确定的模型不要进表（走 Unknown / 用户配置）。
pub const REGISTRY: &[RegistryEntry] = &[
    // OpenAI
    RegistryEntry {
        prefix: "gpt-4.1-nano",
        context_window: 1_047_576,
        max_output_tokens: Some(32_768),
    },
    RegistryEntry {
        prefix: "gpt-4.1-mini",
        context_window: 1_047_576,
        max_output_tokens: Some(32_768),
    },
    RegistryEntry {
        prefix: "gpt-4.1",
        context_window: 1_047_576,
        max_output_tokens: Some(32_768),
    },
    RegistryEntry {
        prefix: "gpt-4o-mini",
        context_window: 128_000,
        max_output_tokens: Some(16_384),
    },
    RegistryEntry {
        prefix: "gpt-4o",
        context_window: 128_000,
        max_output_tokens: Some(16_384),
    },
    RegistryEntry {
        prefix: "gpt-4-turbo",
        context_window: 128_000,
        max_output_tokens: Some(4_096),
    },
    RegistryEntry {
        prefix: "gpt-4",
        context_window: 8_192,
        max_output_tokens: None,
    },
    RegistryEntry {
        prefix: "gpt-3.5-turbo",
        context_window: 16_385,
        max_output_tokens: Some(4_096),
    },
    RegistryEntry {
        prefix: "o4-mini",
        context_window: 200_000,
        max_output_tokens: Some(100_000),
    },
    RegistryEntry {
        prefix: "o3-mini",
        context_window: 200_000,
        max_output_tokens: Some(100_000),
    },
    RegistryEntry {
        prefix: "o3",
        context_window: 200_000,
        max_output_tokens: Some(100_000),
    },
    // Anthropic
    RegistryEntry {
        prefix: "claude-opus-4",
        context_window: 200_000,
        max_output_tokens: Some(32_000),
    },
    RegistryEntry {
        prefix: "claude-sonnet-4",
        context_window: 200_000,
        max_output_tokens: Some(64_000),
    },
    RegistryEntry {
        prefix: "claude-3-7-sonnet",
        context_window: 200_000,
        max_output_tokens: Some(64_000),
    },
    RegistryEntry {
        prefix: "claude-3-5-sonnet",
        context_window: 200_000,
        max_output_tokens: Some(8_192),
    },
    RegistryEntry {
        prefix: "claude-3-5-haiku",
        context_window: 200_000,
        max_output_tokens: Some(8_192),
    },
    RegistryEntry {
        prefix: "claude-3-opus",
        context_window: 200_000,
        max_output_tokens: Some(4_096),
    },
    RegistryEntry {
        prefix: "claude-3-haiku",
        context_window: 200_000,
        max_output_tokens: Some(4_096),
    },
    // Google Gemini
    RegistryEntry {
        prefix: "gemini-2.5-pro",
        context_window: 1_048_576,
        max_output_tokens: Some(65_536),
    },
    RegistryEntry {
        prefix: "gemini-2.5-flash",
        context_window: 1_048_576,
        max_output_tokens: Some(65_536),
    },
    RegistryEntry {
        prefix: "gemini-2.0-flash",
        context_window: 1_048_576,
        max_output_tokens: Some(8_192),
    },
    RegistryEntry {
        prefix: "gemini-1.5-pro",
        context_window: 2_097_152,
        max_output_tokens: Some(8_192),
    },
    RegistryEntry {
        prefix: "gemini-1.5-flash",
        context_window: 1_048_576,
        max_output_tokens: Some(8_192),
    },
];

/// Registry 查找：最长前缀匹配；`provider/model` 先取最后一段。
pub fn registry_lookup(model: &str) -> Option<&'static RegistryEntry> {
    let lower = model.to_ascii_lowercase();
    let suffix = lower.rsplit('/').next().unwrap_or(lower.as_str());
    REGISTRY
        .iter()
        .filter(|entry| lower.starts_with(entry.prefix) || suffix.starts_with(entry.prefix))
        .max_by_key(|entry| entry.prefix.len())
}

/// 一次网络探测的原始结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Probe {
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    runtime_limit: Option<u64>,
}

/// 缓存 key：配置里任何影响解析结果的字段都参与，切换模型/修改覆盖后旧条目不会误用。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    base_url: String,
    model: String,
    user_limit: Option<u64>,
    max_output_tokens: Option<u64>,
}

impl CacheKey {
    fn of(cfg: &ModelConfig) -> Self {
        Self {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            user_limit: cfg.context_window,
            max_output_tokens: cfg.max_output_tokens,
        }
    }
}

/// 带缓存的模型能力解析器。
///
/// 内部只有一个 `std::sync::Mutex`：锁内只做 HashMap 读写，绝不跨 `await` 持有。
pub struct ModelCapabilityResolver {
    cache: Mutex<HashMap<CacheKey, ModelCapabilities>>,
    http: Option<reqwest::Client>,
    /// 通用目录只加载一次（首次 `resolve` 时读缓存/拉取 models.dev）。
    catalog: OnceCell<Option<Arc<ModelCatalog>>>,
    /// 目录缓存路径；`None` = 不联网、不缓存（测试/独立使用）。
    catalog_cache: Option<PathBuf>,
}

impl ModelCapabilityResolver {
    /// 生产构造：建一个 1.2s 超时的 HTTP 客户端；构造失败时只走静态解析。
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .ok();
        Self {
            cache: Mutex::new(HashMap::new()),
            http,
            catalog: OnceCell::new(),
            catalog_cache: None,
        }
    }

    /// 带模型目录缓存的构造：启动/切换模型时读 `~/.ignis/model_catalog.json`，
    /// 过期则拉取 models.dev；离线时回退到旧缓存/内置 Registry。
    pub fn with_catalog_cache(path: PathBuf) -> Self {
        let mut resolver = Self::new();
        resolver.catalog_cache = Some(path);
        resolver
    }

    /// 注入一份目录（测试用；不联网、不写缓存）。
    #[cfg(test)]
    pub(crate) fn with_catalog(catalog: ModelCatalog) -> Self {
        let resolver = Self::new();
        let _ = resolver.catalog.set(Some(Arc::new(catalog)));
        resolver
    }

    /// 同步静态解析（Registry + 用户配置）。不发起网络请求。
    pub fn static_capabilities(cfg: &ModelConfig) -> ModelCapabilities {
        ModelCapabilities::static_resolve(cfg)
    }

    /// 加载通用目录：只尝试一次；没有缓存路径时直接返回 `None`。
    async fn catalog(&self) -> Option<Arc<ModelCatalog>> {
        self.catalog
            .get_or_init(|| async {
                let path = self.catalog_cache.as_deref()?;
                let http = self.http.as_ref()?;
                ModelCatalog::load_or_fetch(http, path).await.map(Arc::new)
            })
            .await
            .clone()
    }

    /// 读缓存（不发起请求）。
    pub fn cached(&self, cfg: &ModelConfig) -> Option<ModelCapabilities> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&CacheKey::of(cfg))
            .cloned()
    }

    /// 清空缓存（切换模型/配置后显式失效）。
    pub fn invalidate(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// 解析模型能力：先查缓存，未命中则目录 + 静态解析 + 一次短超时网络探测，再写入缓存。
    pub async fn resolve(&self, cfg: &ModelConfig) -> ModelCapabilities {
        if let Some(hit) = self.cached(cfg) {
            return hit;
        }
        let catalog = self.catalog().await;
        let kind = ProviderKind::detect(cfg);
        let mut caps = ModelCapabilities::static_resolve_with_catalog(cfg, catalog.as_deref());
        if let Some(probe) = self.probe(kind, cfg).await {
            apply_probe(&mut caps, kind, probe);
        }
        self.store(cfg, &caps);
        caps
    }

    /// 写入缓存（测试也用它验证失效语义）。
    fn store(&self, cfg: &ModelConfig, caps: &ModelCapabilities) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(CacheKey::of(cfg), caps.clone());
    }

    /// 一次网络探测：只对已知会返回能力字段的端点发请求。
    async fn probe(&self, kind: ProviderKind, cfg: &ModelConfig) -> Option<Probe> {
        let client = self.http.as_ref()?;
        match kind {
            ProviderKind::OpenRouter => {
                let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
                let value = get_json(client, &url).await?;
                parse_openrouter_models(&value, &cfg.model)
            }
            ProviderKind::Ollama => {
                let url = format!("{}/api/show", ollama_root(&cfg.base_url));
                let value =
                    post_json(client, &url, &serde_json::json!({"model": cfg.model})).await?;
                parse_ollama_show(&value)
            }
            kind if kind.probes_models() => {
                let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
                let value = get_json(client, &url).await?;
                parse_vllm_models(&value, &cfg.model)
            }
            _ => None,
        }
    }
}

impl Default for ModelCapabilityResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// 把探测结果合并进静态能力：Provider API 给原生窗口，Runtime API 给实际限制。
fn apply_probe(caps: &mut ModelCapabilities, kind: ProviderKind, probe: Probe) {
    if let Some(context) = probe.context_window {
        caps.context_window = Some(context);
        caps.source = CapabilitySource::ProviderApi;
    }
    if let Some(max_output) = probe.max_output_tokens {
        caps.max_output_tokens = Some(max_output);
    }
    if let Some(limit) = probe.runtime_limit {
        caps.runtime_limit = Some(limit);
        caps.source = CapabilitySource::Runtime;
        // 本机端点在 `/models` 里给出 `max_model_len` 时，可以确定它是 vLLM。
        if matches!(kind, ProviderKind::Local) {
            caps.provider = "vLLM".to_string();
        }
    }
    recompute_effective(caps);
}

/// 重新计算有效窗口与有效来源。
fn recompute_effective(caps: &mut ModelCapabilities) {
    let effective = [caps.context_window, caps.runtime_limit, caps.user_limit]
        .into_iter()
        .flatten()
        .min();
    caps.effective_limit = effective;
    caps.effective_source = match effective {
        None => CapabilitySource::Unknown,
        Some(limit) => {
            if caps.user_limit == Some(limit) {
                CapabilitySource::UserConfig
            } else if caps.runtime_limit == Some(limit) {
                CapabilitySource::Runtime
            } else {
                caps.source
            }
        }
    };
}

/// Ollama 的根地址：`http://host:11434/v1` → `http://host:11434`。
fn ollama_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    client
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()
}

async fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Option<Value> {
    client
        .post(url)
        .timeout(PROBE_TIMEOUT)
        .json(body)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()
}

/// 解析 OpenRouter `/models`：`context_length` 与 `top_provider.max_completion_tokens`。
fn parse_openrouter_models(value: &Value, model: &str) -> Option<Probe> {
    let entry = find_model(value, model)?;
    let context_window = entry.get("context_length").and_then(Value::as_u64);
    let max_output_tokens = entry
        .pointer("/top_provider/max_completion_tokens")
        .and_then(Value::as_u64);
    if context_window.is_none() && max_output_tokens.is_none() {
        return None;
    }
    Some(Probe {
        context_window,
        max_output_tokens,
        runtime_limit: None,
    })
}

/// 解析 vLLM `/models`：`max_model_len` 是运行时实际限制。
fn parse_vllm_models(value: &Value, model: &str) -> Option<Probe> {
    let entry = find_model(value, model)?;
    let runtime_limit = entry.get("max_model_len").and_then(Value::as_u64);
    runtime_limit.map(|limit| Probe {
        runtime_limit: Some(limit),
        ..Probe::default()
    })
}

/// 解析 Ollama `/api/show`：优先 `parameters` 里的 `num_ctx`（显式运行时配置），
/// 其次 `model_info` 的 `*.context_length`（模型训练窗口）。
fn parse_ollama_show(value: &Value) -> Option<Probe> {
    let runtime_limit = num_ctx(value)
        .or_else(|| model_info_context(value))
        .filter(|limit| *limit > 0);
    runtime_limit.map(|limit| Probe {
        runtime_limit: Some(limit),
        ..Probe::default()
    })
}

/// `parameters` 是 Ollama 的 Modelfile 参数文本（每行 `name value`）。
fn num_ctx(value: &Value) -> Option<u64> {
    let parameters = value.get("parameters").and_then(Value::as_str)?;
    parameters.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("num_ctx"), Some(raw)) => raw.parse::<u64>().ok(),
            _ => None,
        }
    })
}

/// `model_info` 的键带架构前缀（例如 `llama.context_length`）。
fn model_info_context(value: &Value) -> Option<u64> {
    let info = value.get("model_info")?.as_object()?;
    info.iter()
        .find(|(key, _)| key.as_str() == "context_length" || key.ends_with(".context_length"))
        .and_then(|(_, value)| value.as_u64())
}

/// 在 `data[]` 里找模型：先精确匹配，再按 `provider/model` 的后缀匹配。
fn find_model<'a>(value: &'a Value, model: &str) -> Option<&'a Value> {
    let data = value.get("data")?.as_array()?;
    let suffix = model.rsplit('/').next().unwrap_or(model);
    data.iter().find(|entry| {
        entry
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == model || id.rsplit('/').next() == Some(suffix))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CapabilitySource, ModelCapabilities, ModelCapabilityResolver, ProviderKind, REGISTRY,
        RegistryEntry, parse_ollama_show, parse_openrouter_models, parse_vllm_models,
        registry_lookup,
    };
    use crate::ModelConfig;
    use crate::catalog::fixture_catalog;

    fn config(model: &str) -> ModelConfig {
        ModelConfig {
            model: model.to_string(),
            ..ModelConfig::default()
        }
    }

    #[test]
    fn provider_detection_covers_the_supported_shapes() {
        let cases = [
            ("https://api.openai.com/v1", ProviderKind::OpenAi),
            ("https://openrouter.ai/api/v1", ProviderKind::OpenRouter),
            ("https://api.anthropic.com/v1", ProviderKind::Anthropic),
            (
                "https://generativelanguage.googleapis.com/v1beta",
                ProviderKind::Gemini,
            ),
            ("http://localhost:11434/v1", ProviderKind::Ollama),
            ("http://127.0.0.1:8000/v1", ProviderKind::Local),
            ("https://vllm.example.com/v1", ProviderKind::Vllm),
            ("https://proxy.example.com/v1", ProviderKind::Custom),
        ];
        for (base_url, want) in cases {
            let cfg = ModelConfig {
                base_url: base_url.to_string(),
                ..config("m")
            };
            assert_eq!(ProviderKind::detect(&cfg), want, "{base_url}");
        }
    }

    #[test]
    fn registry_uses_the_longest_prefix_and_strips_provider_prefixes() {
        assert_eq!(
            registry_lookup("gpt-4o-mini").map(|entry| entry.context_window),
            Some(128_000)
        );
        assert_eq!(
            registry_lookup("gpt-4o-mini-2024-07-18").map(|entry| entry.context_window),
            Some(128_000)
        );
        assert_eq!(
            registry_lookup("gpt-4.1-mini").map(|entry| entry.context_window),
            Some(1_047_576)
        );
        assert_eq!(
            registry_lookup("anthropic/claude-sonnet-4").map(|entry| entry.context_window),
            Some(200_000)
        );
        assert_eq!(
            registry_lookup("google/gemini-2.5-pro").map(|entry| entry.context_window),
            Some(1_048_576)
        );
        assert_eq!(
            registry_lookup("gpt-4o-mini").map(|entry| entry.max_output_tokens),
            Some(Some(16_384))
        );
        assert_eq!(registry_lookup("unknown-model-xyz"), None);
        // Registry 的条目形状：非空、窗口为正、max output 不超过窗口（发布值自洽）。
        assert!(!REGISTRY.is_empty());
        for entry in REGISTRY {
            let RegistryEntry {
                prefix,
                context_window,
                max_output_tokens,
            } = *entry;
            assert!(!prefix.is_empty());
            assert!(context_window > 0, "{prefix}");
            assert!(
                max_output_tokens.is_none_or(|output| output <= context_window),
                "{prefix}: max output 不应超过上下文窗口"
            );
        }
    }

    #[test]
    fn static_resolution_distinguishes_window_from_max_output() {
        let caps = ModelCapabilities::static_resolve(&config("gpt-4o-mini"));
        assert_eq!(caps.provider, "OpenAI");
        assert_eq!(caps.context_window, Some(128_000));
        assert_eq!(caps.max_output_tokens, Some(16_384));
        assert_eq!(caps.effective_limit, Some(128_000));
        assert_eq!(caps.source, CapabilitySource::Registry);
        assert_eq!(caps.effective_source, CapabilitySource::Registry);
        assert_ne!(
            caps.context_window, caps.max_output_tokens,
            "上下文窗口不能等于最大输出（除非模型恰好如此）"
        );
    }

    #[test]
    fn user_overrides_cap_the_effective_window_and_are_visible() {
        let cfg = ModelConfig {
            context_window: Some(64_000),
            max_output_tokens: Some(4_096),
            ..config("gpt-4o-mini")
        };
        let caps = ModelCapabilities::static_resolve(&cfg);
        assert_eq!(caps.context_window, Some(128_000), "原生窗口照旧展示");
        assert_eq!(caps.user_limit, Some(64_000));
        assert_eq!(caps.effective_limit, Some(64_000));
        assert_eq!(caps.source, CapabilitySource::Registry);
        assert_eq!(
            caps.effective_source,
            CapabilitySource::UserConfig,
            "被用户压低时有效来源是用户配置"
        );
        assert_eq!(caps.max_output_tokens, Some(4_096));
    }

    #[test]
    fn unknown_models_stay_unknown_without_guessing() {
        let caps = ModelCapabilities::static_resolve(&config("totally-unknown"));
        assert_eq!(caps.context_window, None);
        assert_eq!(caps.effective_limit, None);
        assert_eq!(caps.source, CapabilitySource::Unknown);
        assert_eq!(caps.effective_source, CapabilitySource::Unknown);
        assert_eq!(caps.window_word(), "?");

        // 未知模型 + 用户配置：有效限制来自用户，窗口不再显示 `?`。
        let cfg = ModelConfig {
            context_window: Some(32_000),
            ..config("totally-unknown")
        };
        let caps = ModelCapabilities::static_resolve(&cfg);
        assert_eq!(caps.context_window, Some(32_000));
        assert_eq!(caps.effective_limit, Some(32_000));
        assert_eq!(caps.source, CapabilitySource::UserConfig);
        assert_eq!(caps.effective_source, CapabilitySource::UserConfig);
        assert_eq!(caps.window_word(), "32000");
    }

    /// 通用目录（models.dev 形状）直接识别 MiniMax-M3：不再需要单个模型的 Rust 特判。
    #[test]
    fn catalog_resolves_minimax_m3_without_a_hardcoded_entry() {
        let catalog = fixture_catalog();
        let cfg = ModelConfig {
            base_url: "https://api.minimax.cn/v1".into(),
            model: "MiniMax-M3".into(),
            ..ModelConfig::default()
        };
        let caps = ModelCapabilities::static_resolve_with_catalog(&cfg, Some(&catalog));
        assert_eq!(caps.provider, "OpenAI-compatible");
        assert_eq!(caps.context_window, Some(1_000_000));
        assert_eq!(caps.max_output_tokens, Some(512_000));
        assert_eq!(caps.effective_limit, Some(1_000_000));
        assert_eq!(caps.source, CapabilitySource::Catalog);
        assert_eq!(CapabilitySource::Catalog.label(), "Model Catalog");
    }

    /// 目录值优先于内置 Registry；目录缺失时仍能用 Registry 兜底。
    #[test]
    fn catalog_wins_over_the_bundled_registry() {
        let catalog = fixture_catalog();
        let caps =
            ModelCapabilities::static_resolve_with_catalog(&config("gpt-4o-mini"), Some(&catalog));
        assert_eq!(
            caps.context_window,
            Some(200_000),
            "目录的 200K 优先于 128K"
        );
        assert_eq!(caps.max_input_tokens, Some(190_000));
        assert_eq!(caps.max_output_tokens, Some(100_000));
        assert_eq!(caps.source, CapabilitySource::Catalog);

        let fallback = ModelCapabilities::static_resolve(&config("gpt-4o-mini"));
        assert_eq!(fallback.context_window, Some(128_000));
        assert_eq!(fallback.source, CapabilitySource::Registry);
    }

    /// 用户配置仍是上限：目录给出原生窗口，有效值取 min，并标明有效来源。
    #[test]
    fn catalog_user_cap_still_binds_the_effective_limit() {
        let catalog = fixture_catalog();
        let cfg = ModelConfig {
            base_url: "https://api.minimax.cn/v1".into(),
            model: "MiniMax-M3".into(),
            context_window: Some(500_000),
            max_output_tokens: Some(64_000),
            ..ModelConfig::default()
        };
        let caps = ModelCapabilities::static_resolve_with_catalog(&cfg, Some(&catalog));
        assert_eq!(caps.context_window, Some(1_000_000), "原生窗口仍展示目录值");
        assert_eq!(caps.effective_limit, Some(500_000));
        assert_eq!(caps.source, CapabilitySource::Catalog);
        assert_eq!(caps.effective_source, CapabilitySource::UserConfig);
        assert_eq!(caps.max_output_tokens, Some(64_000), "用户输出上限覆盖目录");
    }

    /// 多 Provider 同名模型值冲突：没有 Provider hint 时不猜。
    #[test]
    fn ambiguous_catalog_model_stays_unknown() {
        let catalog = fixture_catalog();
        let cfg = ModelConfig {
            base_url: "https://unknown.example.com/v1".into(),
            model: "MiniMax-M3".into(),
            ..ModelConfig::default()
        };
        let caps = ModelCapabilities::static_resolve_with_catalog(&cfg, Some(&catalog));
        assert_eq!(caps.context_window, None);
        assert_eq!(caps.effective_limit, None);
        assert_eq!(caps.source, CapabilitySource::Unknown);
    }

    #[tokio::test]
    async fn resolver_uses_the_injected_catalog() {
        let resolver = ModelCapabilityResolver::with_catalog(fixture_catalog());
        let cfg = ModelConfig {
            base_url: "https://api.minimax.cn/v1".into(),
            model: "MiniMax-M3".into(),
            ..ModelConfig::default()
        };
        let caps = resolver.resolve(&cfg).await;
        assert_eq!(caps.context_window, Some(1_000_000));
        assert_eq!(caps.effective_limit, Some(1_000_000));
        assert_eq!(caps.source, CapabilitySource::Catalog);
    }

    #[test]
    fn ollama_and_vllm_do_not_use_the_registry() {
        // 同一个名字在 Ollama 上可能是 4K，也可能是 128K：不能拿 Registry 猜。
        let cfg = ModelConfig {
            base_url: "http://localhost:11434/v1".into(),
            ..config("llama3.1")
        };
        let caps = ModelCapabilities::static_resolve(&cfg);
        assert_eq!(caps.provider, "Ollama");
        assert_eq!(caps.context_window, None);
        assert_eq!(caps.effective_limit, None);

        let cfg = ModelConfig {
            base_url: "http://127.0.0.1:8000/v1".into(),
            ..config("gpt-4o-mini")
        };
        let caps = ModelCapabilities::static_resolve(&cfg);
        assert_eq!(caps.provider, "OpenAI-compatible (local)");
        assert_eq!(
            caps.context_window,
            Some(128_000),
            "本机端点先按 Registry 回退"
        );
    }

    #[test]
    fn openrouter_parser_reads_context_and_output_limits() {
        let body = json!({
            "data": [
                {"id": "other/model", "context_length": 8000},
                {
                    "id": "anthropic/claude-sonnet-4",
                    "context_length": 200000,
                    "top_provider": {"max_completion_tokens": 64000}
                }
            ]
        });
        let probe = parse_openrouter_models(&body, "anthropic/claude-sonnet-4").expect("精确命中");
        assert_eq!(probe.context_window, Some(200_000));
        assert_eq!(probe.max_output_tokens, Some(64_000));
        assert_eq!(probe.runtime_limit, None);

        // 配置里只有后缀时也能命中 provider/model。
        assert_eq!(
            parse_openrouter_models(&body, "claude-sonnet-4")
                .and_then(|probe| probe.context_window),
            Some(200_000)
        );
        assert!(parse_openrouter_models(&body, "missing").is_none());
        assert!(
            parse_openrouter_models(&json!({"data": [{"id": "x"}]}), "x").is_none(),
            "没有能力字段时不算探测成功"
        );
    }

    #[test]
    fn vllm_parser_reads_max_model_len_as_a_runtime_limit() {
        let body = json!({
            "object": "list",
            "data": [{"id": "my-model", "max_model_len": 32768}]
        });
        let probe = parse_vllm_models(&body, "my-model").expect("max_model_len");
        assert_eq!(probe.runtime_limit, Some(32_768));
        assert_eq!(probe.context_window, None);
        assert!(parse_vllm_models(&json!({"data": [{"id": "my-model"}]}), "my-model").is_none());
    }

    #[test]
    fn ollama_parser_prefers_num_ctx_then_model_info() {
        let body = json!({
            "parameters": "num_ctx 8192\nstop <|end|>",
            "model_info": {"llama.context_length": 131072}
        });
        assert_eq!(
            parse_ollama_show(&body).and_then(|probe| probe.runtime_limit),
            Some(8_192),
            "显式 num_ctx 优先"
        );

        let body = json!({"model_info": {"qwen2.context_length": 32768}});
        assert_eq!(
            parse_ollama_show(&body).and_then(|probe| probe.runtime_limit),
            Some(32_768)
        );
        assert!(parse_ollama_show(&json!({"model_info": {}})).is_none());
    }

    #[test]
    fn cache_key_changes_with_model_and_overrides() {
        let resolver = ModelCapabilityResolver::new();
        let cfg = config("gpt-4o-mini");
        let caps = ModelCapabilities::static_resolve(&cfg);
        resolver.store(&cfg, &caps);

        assert_eq!(resolver.cached(&cfg), Some(caps.clone()));
        // 换模型：旧条目不命中。
        assert_eq!(resolver.cached(&config("gpt-4.1")), None);
        // 同一模型但用户覆盖变了：旧条目不命中。
        let capped = ModelConfig {
            context_window: Some(32_000),
            ..config("gpt-4o-mini")
        };
        assert_eq!(resolver.cached(&capped), None);

        resolver.invalidate();
        assert_eq!(resolver.cached(&cfg), None);
    }

    #[tokio::test]
    async fn local_endpoint_probe_uses_the_vllm_shape() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本机测试端口");
        let addr = listener.local_addr().expect("测试端口地址");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受一次连接");
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"object":"list","data":[{"id":"local-model","max_model_len":65536}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("写回响应");
        });

        let cfg = ModelConfig {
            base_url: format!("http://{addr}/v1"),
            model: "local-model".into(),
            ..ModelConfig::default()
        };
        let resolver = ModelCapabilityResolver::new();
        let caps = resolver.resolve(&cfg).await;
        assert_eq!(caps.runtime_limit, Some(65_536));
        assert_eq!(caps.effective_limit, Some(65_536));
        assert_eq!(caps.source, CapabilitySource::Runtime);
        assert_eq!(caps.provider, "vLLM");

        // 第二次解析走缓存：不会再发请求（端口只接受一次连接，仍应成功返回）。
        let cached = resolver.resolve(&cfg).await;
        assert_eq!(cached, caps);
    }
}
