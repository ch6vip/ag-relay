use ag_relay_lib::policy_settings::PolicySettingsStore;
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ag-relay-policy-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> PolicySettingsStore {
        PolicySettingsStore::new(self.0.join("policy-settings.json"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn 启动环境_显式添加完整策略快照且保持旧环境默认表() {
    use ag_relay_lib::{
        connection_settings::ConnectionSettings, gateway_env, policy_settings::PolicySettings,
    };
    let fixture = Fixture::new("env");
    let connection = ConnectionSettings::default();
    let diagnostics = fixture.0.join("diagnostics.jsonl");
    let previous = gateway_env::gateway_env_pairs_with_settings(&diagnostics, &connection);
    let mut policy = PolicySettings::default();
    policy.profiles.codex.max_attempts = 17;
    policy.profiles.general.base_delay_ms = 333;
    policy.profiles.general.uncovered_error_fallback = false;
    let mut next =
        gateway_env::gateway_env_pairs_with_policy(&diagnostics, &connection, &policy).unwrap();
    let index = next
        .iter()
        .position(|(key, _)| key == "PROXY_POLICY_JSON")
        .unwrap();
    let (_, encoded) = next.remove(index);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
        serde_json::to_value(&policy).unwrap()
    );
    assert_eq!(next, previous, "17 项默认及诊断路径不能跟着策略变化");
    assert_eq!(next.len(), 19);
    policy.profiles.codex.max_attempts = 0;
    assert!(
        gateway_env::gateway_env_pairs_with_policy(&diagnostics, &connection, &policy).is_err()
    );
}

#[test]
fn 默认策略_吻合跨语言独立字面契约() {
    let fixture = Fixture::new("contract");
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/policy-settings.json")).unwrap();
    assert_eq!(
        serde_json::to_value(fixture.store().load().unwrap()).unwrap(),
        expected
    );
}

#[test]
fn 损坏策略_未知字段模式版本和非整数不可静默回退() {
    let fixture = Fixture::new("malformed");
    let path = fixture.0.join("policy-settings.json");
    for source in [
        "",
        "{",
        "[]",
        "null",
        "{}",
        r#"{"schemaVersion":null}"#,
        r#"{"schemaVersion":1.0}"#,
        r#"{"schemaVersion":2}"#,
        r#"{"schemaVersion":1,"unknown":true}"#,
        r#"{"schemaVersion":1,"profiles":null}"#,
        r#"{"schemaVersion":1,"profiles":{"extra":{}}}"#,
        r#"{"schemaVersion":1,"profiles":{"codex":null}}"#,
        r#"{"schemaVersion":1,"profiles":{"codex":{"unknown":false}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"historyMode":"sometimes"}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"historyMode":null}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"maxAttempts":1.5}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"maxAttempts":-1}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"maxAttempts":4294967296}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"baseDelayMs":1.5}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"baseDelayMs":18446744073709551616}}}"#,
        r#"{"schemaVersion":1,"profiles":{"general":{"rememberSession":"true"}}}"#,
    ] {
        std::fs::write(&path, source).unwrap();
        assert!(fixture.store().load().is_err(), "应拒绝 {source}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
}

#[test]
fn 原子保存_失败不损坏目标且清理临时文件() {
    use ag_relay_lib::policy_settings::PolicySettings;
    let fixture = Fixture::new("replace-failure");
    let path = fixture.0.join("policy-settings.json");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("retained"), "keep").unwrap();
    assert!(fixture.store().save(&PolicySettings::default()).is_err());
    assert_eq!(
        std::fs::read_to_string(path.join("retained")).unwrap(),
        "keep"
    );
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn 原子保存_替换被拒绝仍保留上次有效设置() {
    use ag_relay_lib::policy_settings::PolicySettings;
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new("locked-target");
    let valid = fixture.store().save(&PolicySettings::default()).unwrap();
    // Windows 文件共享边界：允许读取，但不允许删除/替换该文件。
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(fixture.0.join("policy-settings.json"))
        .unwrap();
    let mut changed = valid.clone();
    changed.profiles.codex.max_attempts = 23;
    assert!(fixture.store().save(&changed).is_err());
    assert_eq!(fixture.store().load().unwrap(), valid);
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    drop(locked);
}

#[test]
fn 原子保存_并发读取不遇到半写配置() {
    use ag_relay_lib::policy_settings::PolicySettings;
    let fixture = Fixture::new("atomic");
    fixture.store().save(&PolicySettings::default()).unwrap();
    let writer = fixture.store();
    let thread = std::thread::spawn(move || {
        for attempts in 1..=100 {
            let mut next = PolicySettings::default();
            next.profiles.codex.max_attempts = attempts;
            writer.save(&next).unwrap();
        }
    });
    while !thread.is_finished() {
        fixture
            .store()
            .load()
            .expect("读取应始终得到完整且有效的配置");
    }
    thread.join().unwrap();
    assert_eq!(
        fixture.store().load().unwrap().profiles.codex.max_attempts,
        100
    );
}

#[test]
fn 边界合法值_上下限及关闭依赖选项可保存() {
    use ag_relay_lib::policy_settings::{HistoryMode, PolicySettings};
    let fixture = Fixture::new("valid-boundaries");
    for (attempts, delay, budget, normalize, reasoning) in
        [(1, 1, 1000, 1, 2), (1000, 300000, 3600000, 999, 1000)]
    {
        let mut settings = PolicySettings::default();
        for profile in [&mut settings.profiles.codex, &mut settings.profiles.general] {
            profile.max_attempts = attempts;
            profile.base_delay_ms = delay;
            profile.max_delay_ms = delay;
            profile.total_budget_ms = budget;
            profile.connect_timeout_ms = budget.min(600_000);
            profile.empty_response_normalize_after = normalize;
            profile.empty_response_reasoning_after = reasoning;
            profile.history_mode = HistoryMode::Off;
            profile.remember_session = true;
        }
        settings.profiles.codex.history_confirmations = attempts.min(4);
        settings.profiles.general.history_confirmations = attempts;
        settings.profiles.general.empty_response_repair = false;
        settings.profiles.general.empty_response_drop_reasoning = false;
        fixture.store().save(&settings).unwrap();
        assert_eq!(fixture.store().load().unwrap(), settings);
    }
}

#[test]
fn 非法策略_读写均拒绝并保留上次有效设置() {
    use ag_relay_lib::policy_settings::PolicySettings;
    use serde_json::json;
    let fixture = Fixture::new("invalid");
    let valid = fixture.store().save(&PolicySettings::default()).unwrap();
    let input_path = fixture.0.join("invalid.json");
    let input_store = PolicySettingsStore::new(input_path.clone());
    let mut cases = vec![];
    for group in ["codex", "general"] {
        for (field, field_value) in [
            ("maxAttempts", json!(0)),
            ("maxAttempts", json!(1001)),
            ("baseDelayMs", json!(0)),
            ("baseDelayMs", json!(1001)),
            ("maxDelayMs", json!(0)),
            ("maxDelayMs", json!(300001)),
            ("totalBudgetMs", json!(999)),
            ("totalBudgetMs", json!(3600001)),
            ("connectTimeoutMs", json!(999)),
            ("connectTimeoutMs", json!(600001)),
            ("historyConfirmations", json!(0)),
            ("emptyResponseNormalizeAfter", json!(0)),
            ("emptyResponseNormalizeAfter", json!(1001)),
            ("emptyResponseReasoningAfter", json!(0)),
            ("emptyResponseReasoningAfter", json!(1001)),
            ("emptyResponseReasoningAfter", json!(2)),
        ] {
            let mut value = serde_json::to_value(&valid).unwrap();
            value["profiles"][group][field] = field_value;
            cases.push(value);
        }
        let mut value = serde_json::to_value(&valid).unwrap();
        value["profiles"][group]["maxAttempts"] = json!(2);
        value["profiles"][group]["historyConfirmations"] = json!(3);
        cases.push(value);
    }
    for (group, field, field_value) in [
        ("codex", "historyConfirmations", json!(5)),
        ("general", "historyConfirmations", json!(1001)),
        ("codex", "emptyResponseRepair", json!(true)),
        ("codex", "emptyResponseDropReasoning", json!(true)),
        ("general", "stripItemIds", json!(true)),
        ("general", "retryHistory", json!(true)),
        ("general", "emptyResponseRepair", json!(false)),
    ] {
        let mut value = serde_json::to_value(&valid).unwrap();
        value["profiles"][group][field] = field_value;
        cases.push(value);
    }
    let mut unsupported = serde_json::to_value(&valid).unwrap();
    unsupported["schemaVersion"] = json!(2);
    cases.push(unsupported);
    for value in cases {
        let candidate: PolicySettings = serde_json::from_value(value.clone()).unwrap();
        assert!(
            fixture.store().save(&candidate).is_err(),
            "应拒绝保存 {value}"
        );
        assert_eq!(fixture.store().load().unwrap(), valid);
        std::fs::write(&input_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(input_store.load().is_err(), "应拒绝读取 {value}");
    }
}

#[test]
fn 保存策略_重开后保留值且不修改连接文件() {
    use ag_relay_lib::policy_settings::{HistoryMode, PolicySettings};
    let fixture = Fixture::new("save");
    let connection_path = fixture.0.join("connection-settings.json");
    let connection = r#"{"upstream":"https://example.test","port":19999}"#;
    std::fs::write(&connection_path, connection).unwrap();
    let mut settings = PolicySettings::default();
    settings.profiles.codex.max_attempts = 12;
    settings.profiles.codex.history_confirmations = 4;
    settings.profiles.general.history_mode = HistoryMode::Off;
    // 关闭历史时仍保留用户的记忆选项；有效性由核心按历史模式共同限制。
    settings.profiles.general.remember_session = true;
    let saved = fixture.store().save(&settings).unwrap();
    assert_eq!(saved, settings);
    assert_eq!(fixture.store().load().unwrap(), settings);
    assert_eq!(
        std::fs::read_to_string(connection_path).unwrap(),
        connection
    );
}

#[test]
fn 未覆盖错误保底_仅磁盘读取补缺字段且保留显式关闭() {
    use ag_relay_lib::policy_settings::PolicySettings;
    let fixture = Fixture::new("fallback-migration");
    let path = fixture.0.join("policy-settings.json");
    for group in ["codex", "general"] {
        let mut expected = PolicySettings::default();
        expected.profiles.codex.uncovered_error_fallback = false;
        expected.profiles.general.uncovered_error_fallback = false;
        let mut old = serde_json::to_value(&expected).unwrap();
        old["profiles"][group]
            .as_object_mut()
            .unwrap()
            .remove("uncoveredErrorFallback");
        assert!(serde_json::from_value::<PolicySettings>(old.clone()).is_err(),
            "严格 DTO 不能补默认冒充生效：{group}");
        let source = serde_json::to_vec(&old).unwrap();
        std::fs::write(&path, &source).unwrap();
        if group == "codex" {
            expected.profiles.codex.uncovered_error_fallback = true;
        } else {
            expected.profiles.general.uncovered_error_fallback = true;
        }
        assert_eq!(fixture.store().load().unwrap(), expected);
        assert_eq!(std::fs::read(&path).unwrap(), source, "迁移不写回旧配置");
    }
}

#[test]
fn 未覆盖错误保底_错误类型不可迁移或反序列化() {
    use ag_relay_lib::policy_settings::PolicySettings;
    use serde_json::json;
    let fixture = Fixture::new("fallback-types");
    let path = fixture.0.join("policy-settings.json");
    for group in ["codex", "general"] {
        for invalid in [json!(null), json!("false"), json!(0), json!([]), json!({})] {
            let mut value = serde_json::to_value(PolicySettings::default()).unwrap();
            value["profiles"][group]["uncoveredErrorFallback"] = invalid;
            assert!(serde_json::from_value::<PolicySettings>(value.clone()).is_err());
            let source = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &source).unwrap();
            assert!(fixture.store().load().is_err(), "应拒绝 {value}");
            assert_eq!(std::fs::read(&path).unwrap(), source);
        }
    }
}

#[test]
fn 未覆盖错误保底_所有历史模式均保存并回读原选择() {
    use ag_relay_lib::policy_settings::{HistoryMode, PolicySettings};
    let fixture = Fixture::new("fallback-modes");
    for mode in [HistoryMode::Off, HistoryMode::Always, HistoryMode::OnError] {
        for enabled in [true, false] {
            let mut settings = PolicySettings::default();
            for profile in [&mut settings.profiles.codex, &mut settings.profiles.general] {
                profile.history_mode = mode;
                profile.uncovered_error_fallback = enabled;
            }
            assert_eq!(fixture.store().save(&settings).unwrap(), settings);
            assert_eq!(fixture.store().load().unwrap(), settings, "模式暂停不清空选择");
        }
    }
}

#[test]
fn 当前版本缺字段_只补对应分组默认且不写回文件() {
    let fixture = Fixture::new("partial");
    let source = r#"{"schemaVersion":1,"profiles":{"codex":{"maxAttempts":9},"general":{"historyMode":"off"}}}"#;
    let path = fixture.0.join("policy-settings.json");
    std::fs::write(&path, source).unwrap();
    let loaded = fixture.store().load().unwrap();
    assert_eq!(loaded.profiles.codex.max_attempts, 9);
    assert!(loaded.profiles.codex.strip_item_ids);
    assert!(!loaded.profiles.codex.empty_response_repair);
    assert!(!loaded.profiles.general.strip_item_ids);
    assert!(loaded.profiles.general.empty_response_repair);
    assert!(loaded.profiles.general.remember_session);
    assert!(loaded.profiles.codex.uncovered_error_fallback);
    assert!(loaded.profiles.general.uncovered_error_fallback);
    assert_eq!(
        serde_json::to_value(&loaded).unwrap()["profiles"]["general"]["historyMode"],
        "off"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    std::fs::write(path, r#"{"schemaVersion":1}"#).unwrap();
    assert_eq!(
        fixture.store().load().unwrap(),
        ag_relay_lib::policy_settings::PolicySettings::default()
    );
}

#[test]
fn 策略缺失_只读返回分组默认且不创建文件() {
    let fixture = Fixture::new("missing");
    let settings = serde_json::to_value(fixture.store().load().unwrap()).unwrap();
    assert_eq!(settings["schemaVersion"], 1);
    for group in ["codex", "general"] {
        let profile = &settings["profiles"][group];
        assert_eq!(profile["maxAttempts"], 110);
        assert_eq!(profile["baseDelayMs"], 500);
        assert_eq!(profile["maxDelayMs"], 1000);
        assert_eq!(profile["totalBudgetMs"], 500000);
        assert_eq!(profile["connectTimeoutMs"], 220000);
        assert_eq!(profile["historyMode"], "on_error");
        assert_eq!(profile["historyConfirmations"], 1);
        assert_eq!(profile["uncoveredErrorFallback"], true);
        assert_eq!(profile["rememberSession"], true);
        assert_eq!(profile["repairStatus"], true);
        assert_eq!(profile["stripItemIds"], group == "codex");
        assert_eq!(profile["retryHistory"], group == "codex");
        assert_eq!(profile["emptyResponseRepair"], group == "general");
        assert_eq!(profile["emptyResponseDropReasoning"], group == "general");
        assert_eq!(profile["emptyResponseNormalizeAfter"], 2);
        assert_eq!(profile["emptyResponseReasoningAfter"], 6);
    }
    assert!(!fixture.0.join("policy-settings.json").exists());
}
