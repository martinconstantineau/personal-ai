//! V5e tests: App Operator tools — `apps.share` mints a scoped
//! capability token and `apps.backup` snapshots app state, both through
//! the injected `AppOperator` surface (PRD §6.8). Security-sensitive:
//! both tools gate on `AskUser` permissions by default.

use pai_agent::appops::StoreAppOperator;
use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_permissions::{Permission, RiskLevel};
use pai_share::{Action, ShareStore};
use pai_storage::Store;
use pai_sync::{backup, crypto, pair};
use pai_tools::{AppOperator, ExecutionMode, Tool, ToolContext, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5e-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    user: User,
    device: Device,
}

fn dev(tag: &str) -> Dev {
    let dir = tmpdir(tag);
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let ids = IdentityStore::new(store.clone());
    let key_dir = dir.join("keys");
    let user = ids.create_user("u").unwrap();
    let device = ids
        .register_device(
            user.id,
            tag,
            Platform::Linux,
            DeviceCapabilities::default(),
            &key_dir,
        )
        .unwrap();
    Dev {
        dir,
        store,
        ids,
        key_dir,
        user,
        device,
    }
}

fn pair_devices(offerer: &Dev, acceptor: &Dev) {
    let agree_a = crypto::agreement_key(offerer.device.id, &offerer.dir).unwrap();
    let agree_b = crypto::agreement_key(acceptor.device.id, &acceptor.dir).unwrap();
    let offer =
        pair::make_offer(&offerer.device, &agree_a, &offerer.ids, &offerer.key_dir).unwrap();
    let accept = pair::accept_offer(
        &acceptor.store,
        &offer,
        &acceptor.device,
        &agree_b,
        &acceptor.ids,
        &acceptor.key_dir,
        &acceptor.dir,
    )
    .unwrap();
    pair::complete_pairing(&offerer.store, &accept, &agree_a, &offerer.dir).unwrap();
}

const MANIFEST: &str = r#"
[app]
name = "Operator Test App"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"
"#;

fn deploy(d: &Dev) -> String {
    let dir = d.dir.join(format!("src-pkg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let pkg = AppPackage::load(&dir).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let devices = d.ids.list_devices(d.user.id).unwrap();
    let signer = pkg.verify_any(&d.ids, &devices).unwrap();
    let dev = devices.iter().find(|x| x.id == signer).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, dev, false)
        .unwrap();
    pkg.manifest.app_id()
}

fn ctx<'a>(apps: Option<&'a dyn AppOperator>) -> ToolContext<'a> {
    ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        vision: None,
        notify: None,
        apps,
        allowed_roots: &[],
    }
}

/// A stub operator that records calls — isolates tool-level arg
/// handling from the store plumbing.
type GrantCall = (String, Vec<String>, i64, Option<String>);

struct StubOps {
    granted: std::sync::Mutex<Vec<GrantCall>>,
    backed_up: std::sync::Mutex<Vec<String>>,
}

impl StubOps {
    fn new() -> Self {
        Self {
            granted: std::sync::Mutex::new(Vec::new()),
            backed_up: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl AppOperator for StubOps {
    fn share_grant(
        &self,
        app_id: &str,
        actions: &[String],
        days: i64,
        for_device: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.granted.lock().unwrap().push((
            app_id.to_string(),
            actions.to_vec(),
            days,
            for_device.map(String::from),
        ));
        Ok(serde_json::json!({
            "token_id": "stub-tok",
            "app_id": app_id,
            "actions": actions,
            "expires": 9999999999i64,
            "grantee_key": null,
            "token": {"v": 1},
        }))
    }

    fn backup(&self, app_id: &str) -> Result<serde_json::Value> {
        self.backed_up.lock().unwrap().push(app_id.to_string());
        Ok(serde_json::json!({"app_id": app_id, "path": "/tmp/x.pak"}))
    }
}

#[tokio::test]
async fn tool_descriptors_gate_on_permissions() {
    let mut reg = ToolRegistry::default();
    reg.register(Arc::new(pai_tools::AppsShareTool));
    reg.register(Arc::new(pai_tools::AppsBackupTool));

    let share = reg.get("apps.share").unwrap();
    let d = share.descriptor();
    assert_eq!(d.required_permissions, vec![Permission::AppShare]);
    assert_eq!(d.risk, RiskLevel::High);
    assert_eq!(d.execution, ExecutionMode::SideEffecting);

    let bkp = reg.get("apps.backup").unwrap();
    let d = bkp.descriptor();
    assert_eq!(d.required_permissions, vec![Permission::AppBackup]);
    assert_eq!(d.risk, RiskLevel::Medium);
    assert_eq!(d.execution, ExecutionMode::SideEffecting);
}

#[tokio::test]
async fn share_schema_validation() {
    let tool = pai_tools::AppsShareTool;
    let d = tool.descriptor();
    // app_id required.
    assert!(pai_tools::validate_args(&d.input_schema, &serde_json::json!({})).is_err());
    // actions must be an array, days an integer, for_device a string.
    assert!(pai_tools::validate_args(
        &d.input_schema,
        &serde_json::json!({"app_id":"a","actions":"exec"})
    )
    .is_err());
    assert!(pai_tools::validate_args(
        &d.input_schema,
        &serde_json::json!({"app_id":"a","days":"30"})
    )
    .is_err());
    pai_tools::validate_args(
        &d.input_schema,
        &serde_json::json!({"app_id":"a","actions":["exec"],"days":7,"for_device":"x"}),
    )
    .unwrap();
}

#[tokio::test]
async fn tools_error_without_operator() {
    let ctx = ctx(None);
    let e = pai_tools::AppsShareTool
        .execute(serde_json::json!({"app_id":"a"}), &ctx)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no app operator surface wired"));
    let e = pai_tools::AppsBackupTool
        .execute(serde_json::json!({"app_id":"a"}), &ctx)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no app operator surface wired"));
}

#[tokio::test]
async fn share_tool_passes_args_to_operator() {
    let ops = StubOps::new();
    let ctx = ctx(Some(&ops));
    let out = pai_tools::AppsShareTool
        .execute(
            serde_json::json!({
                "app_id": "notes", "actions": ["exec","read"],
                "days": 14, "for_device": "peer-x"
            }),
            &ctx,
        )
        .await
        .unwrap();
    {
        let g = ops.granted.lock().unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, "notes");
        assert_eq!(g[0].1, vec!["exec".to_string(), "read".to_string()]);
        assert_eq!(g[0].2, 14);
        assert_eq!(g[0].3.as_deref(), Some("peer-x"));
    }
    assert_eq!(out.value["token_id"], "stub-tok");

    // Defaults: actions=[exec], days=30, for_device=None.
    pai_tools::AppsShareTool
        .execute(serde_json::json!({"app_id":"n2"}), &ctx)
        .await
        .unwrap();
    let g = ops.granted.lock().unwrap();
    assert_eq!(g[1].1, vec!["exec".to_string()]);
    assert_eq!(g[1].2, 30);
    assert!(g[1].3.is_none());
}

#[tokio::test]
async fn backup_tool_passes_app_id() {
    let ops = StubOps::new();
    let ctx = ctx(Some(&ops));
    let out = pai_tools::AppsBackupTool
        .execute(serde_json::json!({"app_id":"db"}), &ctx)
        .await
        .unwrap();
    assert_eq!(ops.backed_up.lock().unwrap()[0], "db");
    assert_eq!(out.value["path"], "/tmp/x.pak");
}

/// End-to-end through `StoreAppOperator`: minted token verifies via the
/// real `ShareStore`, backup lands on disk as a restorable pak.
#[tokio::test]
async fn store_operator_mints_real_capability() {
    let (a, b) = (dev("a"), dev("b"));
    pair_devices(&b, &a);
    let app_id = deploy(&a);
    let ops = StoreAppOperator::new(a.store.clone(), a.dir.clone(), a.device.id);

    // Bearer grant.
    let v = ops
        .share_grant(&app_id, &["exec".into(), "read".into()], 7, None)
        .unwrap();
    let cap: pai_share::Capability =
        pai_share::Capability::from_json(v["token"].as_str().unwrap()).unwrap();
    assert_eq!(cap.app_id, app_id);
    assert!(cap.actions.contains(&Action::Exec));
    assert!(cap.actions.contains(&Action::Read));
    assert!(cap.grantee_key.is_none());
    // Verifies against the issuer's public key.
    let issuer_pk: [u8; 32] = a.device.public_key.as_slice().try_into().unwrap();
    ShareStore::new(&a.dir)
        .verify(&a.ids, &cap, &issuer_pk, Action::Exec)
        .unwrap();

    // Device-bound grant resolved from a peer prefix.
    let prefix = &b.device.id.to_string()[..8];
    let v = ops
        .share_grant(&app_id, &["exec".into()], 30, Some(prefix))
        .unwrap();
    let cap: pai_share::Capability =
        pai_share::Capability::from_json(v["token"].as_str().unwrap()).unwrap();
    assert_eq!(
        cap.grantee_key.as_deref(),
        Some(hex::encode(b.device.public_key.as_slice()).as_str())
    );

    // Unknown app + bad action + bad peer prefix all refuse.
    assert!(ops.share_grant("ghost", &["exec".into()], 7, None).is_err());
    assert!(ops
        .share_grant(&app_id, &["delete".into()], 7, None)
        .is_err());
    assert!(ops
        .share_grant(&app_id, &["exec".into()], 7, Some("zzzz"))
        .is_err());

    // Token landed in share/caps (grants list picks it up).
    let store = ShareStore::new(&a.dir);
    assert!(store
        .list()
        .unwrap()
        .iter()
        .any(|(c, _)| c.token_id == cap.token_id));
}

#[tokio::test]
async fn store_operator_backups_real_pak() {
    let a = dev("a");
    let app_id = deploy(&a);
    let ops = StoreAppOperator::new(a.store.clone(), a.dir.clone(), a.device.id);

    let v = ops.backup(&app_id).unwrap();
    let p = Path::new(v["path"].as_str().unwrap());
    assert!(p.is_file(), "backup pak should exist: {p:?}");
    // Row registered so `apps restore` can find it.
    assert!(backup::list(&a.store)
        .unwrap()
        .iter()
        .any(|r| r.app_id == app_id && r.writer == a.device.id.to_string()));

    assert!(ops.backup("ghost").is_err());
}
