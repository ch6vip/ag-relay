//! 重试与剥离策略：独立版本化设置、分组默认与校验；不包含连接信息或凭据。
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    Off,
    OnError,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyProfile {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub total_budget_ms: u64,
    /// 单次尝试等待上游响应头的上限（毫秒）；请求体更大时核心还会按体积追加。
    pub connect_timeout_ms: u64,
    pub history_mode: HistoryMode,
    pub history_confirmations: u32,
    /// 仅报错后模式、明确识别的 Claude / Codex 请求参与；其他模式保留选择。
    pub uncovered_error_fallback: bool,
    pub strip_item_ids: bool,
    pub remember_session: bool,
    pub repair_status: bool,
    pub retry_history: bool,
    pub empty_response_repair: bool,
    pub empty_response_drop_reasoning: bool,
    pub empty_response_normalize_after: u32,
    pub empty_response_reasoning_after: u32,
}

impl PolicyProfile {
    fn validate(&self, codex: bool) -> Result<(), String> {
        let group = if codex { "Codex" } else { "Claude / Zcode" };
        let fail = |message: &str| Err(format!("{group}：{message}"));
        if !(1..=1000).contains(&self.max_attempts) {
            return fail("常规尝试次数须为 1 到 1000 的整数");
        }
        let confirmations_max = if codex { 4 } else { 1000 };
        if !(1..=confirmations_max).contains(&self.history_confirmations)
            || self.history_confirmations > self.max_attempts
        {
            return fail(if codex {
                "历史确认次数须为 1 到 4，且不能大于常规尝试次数"
            } else {
                "历史确认次数须为 1 到 1000，且不能大于常规尝试次数"
            });
        }
        if self.base_delay_ms == 0
            || self.base_delay_ms > self.max_delay_ms
            || self.max_delay_ms > 300_000
        {
            return fail("等待时间须为正整数毫秒，起始等待不能大于最长等待，最长不超过 300 秒");
        }
        if !(1000..=3_600_000).contains(&self.total_budget_ms) {
            return fail("总时间预算须为 1000 到 3600000 毫秒");
        }
        if !(1000..=600_000).contains(&self.connect_timeout_ms) {
            return fail("单次等待上限须为 1 到 600 秒");
        }
        if self.connect_timeout_ms > self.total_budget_ms {
            return fail("单次等待上限不能大于总时间预算");
        }
        if !(1..=1000).contains(&self.empty_response_normalize_after)
            || !(1..=1000).contains(&self.empty_response_reasoning_after)
            || self.empty_response_reasoning_after <= self.empty_response_normalize_after
        {
            return fail("空回应门槛须为 1 到 1000，删除旧推理必须晚于字段规范化");
        }
        if self.empty_response_drop_reasoning && !self.empty_response_repair {
            return fail("删除旧推理须先启用空回应分级兼容");
        }
        if codex && (self.empty_response_repair || self.empty_response_drop_reasoning) {
            return fail("不支持启用通用组空回应分级兼容");
        }
        if !codex && (self.strip_item_ids || self.retry_history) {
            return fail("不支持 Codex 专属编号清理或保留历史再试");
        }
        Ok(())
    }

    fn defaults(codex: bool) -> Self {
        // 桌面重试默认只从现有 17 项表取值；不更改裸核心的默认或旧环境入口。
        let retry_default = |key| {
            crate::gateway_env::GATEWAY_ENV_TABLE
                .iter()
                .find(|(name, _)| *name == key)
                .expect("桌面重试默认必须存在于环境表")
                .1
                .parse::<u64>()
                .expect("桌面重试默认必须为整数")
        };
        Self {
            max_attempts: retry_default("PROXY_MAX_ATTEMPTS") as u32,
            base_delay_ms: retry_default("PROXY_BASE_DELAY_MS"),
            max_delay_ms: retry_default("PROXY_MAX_DELAY_MS"),
            total_budget_ms: retry_default("PROXY_TOTAL_BUDGET_MS"),
            connect_timeout_ms: retry_default("PROXY_CONNECT_TIMEOUT_MS"),
            history_mode: HistoryMode::OnError,
            history_confirmations: 1,
            uncovered_error_fallback: true,
            strip_item_ids: codex,
            remember_session: true,
            repair_status: true,
            retry_history: codex,
            empty_response_repair: !codex,
            empty_response_drop_reasoning: !codex,
            empty_response_normalize_after: 2,
            empty_response_reasoning_after: 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfiles {
    pub codex: PolicyProfile,
    pub general: PolicyProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicySettings {
    pub schema_version: u32,
    pub profiles: PolicyProfiles,
}

impl Default for PolicySettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            profiles: PolicyProfiles {
                codex: PolicyProfile::defaults(true),
                general: PolicyProfile::defaults(false),
            },
        }
    }
}

impl PolicySettings {
    /// 保存、启动与读取核心有效快照共用校验，关闭模块也不放宽配置格式。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("策略设置版本不受支持，仅支持 schemaVersion 1".into());
        }
        self.profiles.codex.validate(true)?;
        self.profiles.general.validate(false)
    }
}

/// active 只来自当前核心完整且有效的 status 快照；磁盘保存成功不代表运行生效。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicySettingsView {
    pub defaults: PolicySettings,
    pub saved: PolicySettings,
    pub active: Option<PolicySettings>,
    pub active_revision: Option<String>,
    pub restart_required: bool,
    pub active_supported: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivePolicy {
    pub revision: String,
    pub settings: PolicySettings,
}

impl ActivePolicy {
    pub(crate) fn from_status(body: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        // 核心快照不能像磁盘迁移那样补缺省，否则会把未知字段假装为已生效。
        let active: Self =
            serde_json::from_value(value.as_object()?.get("policy")?.clone()).ok()?;
        // schema 1 的核心修订标识固定为 SHA-256；只校验格式，不在 Rust 重算。
        if active.revision.len() != 64
            || !active.revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        active.settings.validate().ok()?;
        Some(active)
    }
}

pub struct PolicySettingsStore {
    path: PathBuf,
}

impl PolicySettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn save(&self, settings: &PolicySettings) -> Result<PolicySettings, String> {
        settings.validate()?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|_| "无法创建策略设置目录")?;
        }
        let bytes = serde_json::to_vec_pretty(settings).map_err(|_| "无法编码策略设置")?;
        crate::atomic_write::replace(&self.path, &bytes)
            .map_err(|_| "无法保存策略设置，原设置保持不变")?;
        Ok(settings.clone())
    }

    pub fn load(&self) -> Result<PolicySettings, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let mut saved: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|_| "策略设置文件无效，请检查 JSON 格式")?;
                if saved
                    .get("schemaVersion")
                    .and_then(serde_json::Value::as_u64)
                    != Some(1)
                {
                    return Err("策略设置版本不受支持，仅支持 schemaVersion 1".into());
                }
                // 仅文件迁移补缺省字段；显式 null、错类型与未知字段保留给严格 DTO 拒绝。
                // 逐层以所在分组的默认值填补，不能给 Codex 套通用组默认。
                fn fill_missing(value: &mut serde_json::Value, defaults: serde_json::Value) {
                    if let (Some(object), serde_json::Value::Object(defaults)) =
                        (value.as_object_mut(), defaults)
                    {
                        for (key, default) in defaults {
                            match object.get_mut(&key) {
                                Some(existing) => fill_missing(existing, default),
                                None => {
                                    object.insert(key, default);
                                }
                            }
                        }
                    }
                }
                fill_missing(
                    &mut saved,
                    serde_json::to_value(PolicySettings::default()).unwrap(),
                );
                let settings: PolicySettings = serde_json::from_value(saved)
                    .map_err(|_| "策略设置文件无效，包含未知字段、模式或非法类型")?;
                settings.validate()?;
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(PolicySettings::default())
            }
            Err(_) => Err("无法读取策略设置文件".into()),
        }
    }
}
