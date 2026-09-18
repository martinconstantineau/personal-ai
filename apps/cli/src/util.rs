//! Cross-command helpers: transports, peer/QR display, guest calls,
//! small input/audit conveniences shared by the command modules.

use crate::ctx::Ctx;
use pai_core::*;
use pai_storage::Store;
use std::sync::Arc;

/// `app.user.devices` name layer: slugify a display name into the DNS
/// label the gateway recognizes (`Alice's phone` -> `alice-s-phone`).
pub(crate) fn name_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Host-header name route — `<app>.<user>.devices` (port ignored)
/// resolves to app id `app` when `<user>` is this device's user slug.
/// The app id may itself be dotted (`com.example.app`).
pub(crate) fn parse_app_name(host: &str, user_slug: &str) -> Option<String> {
    let host = host
        .split(':')
        .next()
        .unwrap_or(host)
        .trim_end_matches('.')
        .to_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 3 || *labels.last()? != "devices" {
        return None;
    }
    if labels[labels.len() - 2] != user_slug {
        return None;
    }
    let app = labels[..labels.len() - 2].join(".");
    (!app.is_empty()).then_some(app)
}

/// Strip control characters from untrusted display fields (device
/// names in pairing files and announcements are attacker-influenced —
/// raw prints could carry terminal escapes).
pub(crate) fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

pub(crate) fn parse_uuid(s: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| Error::InvalidInput(format!("invalid {what} id '{s}'")))
}

/// "--at" accepts RFC3339 or "+N" seconds from now.
pub(crate) fn parse_at(s: &str) -> Result<Timestamp> {
    if let Some(secs) = s.strip_prefix('+').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now() + chrono::Duration::seconds(secs));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| Error::InvalidInput(format!("bad --at '{s}' (RFC3339 or +N secs): {e}")))
}

/// Resolve --dir / --relay into a transport; mutually exclusive.
pub(crate) fn sync_transport(
    dir: &Option<String>,
    relay: &Option<String>,
    token: &Option<String>,
) -> Result<Box<dyn pai_sync::SyncTransport>> {
    match (dir, relay) {
        (Some(d), None) => Ok(Box::new(pai_sync::FolderTransport::new(d.into())?)),
        (None, Some(r)) => Ok(Box::new(pai_sync::relay::RelayTransport::new(
            r,
            token.clone(),
        ))),
        (None, None) => Err(Error::InvalidInput("specify --dir or --relay".into())),
        (Some(_), Some(_)) => Err(Error::InvalidInput(
            "--dir and --relay are mutually exclusive".into(),
        )),
    }
}

/// Discover a paired peer's relay on the LAN and build an
/// authenticated transport to it: bearer token = hex(peer_key), the
/// pairing-derived secret — no shared token file needed.
pub(crate) fn lan_transport(
    to: &Option<String>,
    store: &Store,
    ids: &pai_identity::IdentityStore,
    device: &Device,
    data_dir: &std::path::Path,
) -> Result<Box<dyn pai_sync::SyncTransport>> {
    use pai_sync::crypto;
    let bind = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        pai_mesh::MULTICAST_PORT,
    );
    let sock = pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP))?;
    let found = pai_mesh::discover(&sock, std::time::Duration::from_secs(3));
    let paired = pai_mesh::paired_announcements(store, ids, found)?;
    let target = match to {
        Some(prefix) => paired
            .iter()
            .find(|p| p.peer.device_id.to_string().starts_with(prefix.as_str())),
        None => paired.first(),
    }
    .ok_or_else(|| {
        Error::NotFound(
            "no paired mesh peer announcing — run `pai sync serve --announce` on it".into(),
        )
    })?;
    let agree = crypto::agreement_key(device.id, data_dir)?;
    let token = pai_mesh::token_for(&agree.secret, &target.peer);
    println!(
        "mesh: {} ({}) at http://{}",
        target.peer.name, target.peer.device_id, target.relay_addr
    );
    Ok(Box::new(pai_sync::relay::RelayTransport::new(
        format!("http://{}", target.relay_addr),
        Some(token),
    )))
}

/// Match a device-id prefix against paired peers.
/// Per-device placement weight for `find_peer` — the `place_weight.<id>`
/// meta keys set by `pai broker prefer`. Missing/unparseable = 0.
pub(crate) fn place_weights(
    store: &Arc<Store>,
) -> Box<dyn Fn(&DeviceId) -> i64 + Send + Sync + 'static> {
    let st = store.clone();
    Box::new(move |id| {
        st.meta_get(&format!("place_weight.{id}"))
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Render `payload` as a QR in the terminal — two modules per
/// character via half-blocks, 2-module quiet zone.
pub(crate) fn print_qr(payload: &str) -> Result<()> {
    let code = qrcode::QrCode::new(payload.as_bytes())
        .map_err(|e| Error::Other(format!("qr encode (payload {}B): {e}", payload.len())))?;
    let w = code.width();
    let colors = code.to_colors();
    let at = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w as i32 || y >= w as i32 {
            false
        } else {
            colors[y as usize * w + x as usize] == qrcode::Color::Dark
        }
    };
    for y in ((-2)..(w as i32 + 2)).step_by(2) {
        let mut line = String::with_capacity(w + 8);
        for x in -2..(w as i32 + 2) {
            let ch = match (at(x, y), at(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            line.push(ch);
        }
        println!("{line}");
    }
    Ok(())
}

pub(crate) fn resolve_peer(store: &Store, prefix: &str) -> Result<DeviceId> {
    use pai_sync::pair;
    let peers = pair::list_peers(store)?;
    let matches: Vec<_> = peers
        .iter()
        .filter(|p| p.device_id.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(Error::NotFound(format!(
            "no paired device matching '{prefix}' — `pai broker devices`"
        ))),
        1 => Ok(matches[0].device_id),
        _ => Err(Error::InvalidInput(format!(
            "'{prefix}' matches {} devices — be more specific",
            matches.len()
        ))),
    }
}

/// Everything `guest_call` needs from `base()` — keeps the signature
/// under clippy's arg limit.
pub(crate) struct GuestCtx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) ids: &'a pai_identity::IdentityStore,
    pub(crate) key_dir: &'a std::path::Path,
    pub(crate) device: &'a Device,
}

/// Guest-side request common to `apps run/read/write --cap`: load the
/// token, check it names `want_app`, resolve the target device, and
/// sign when the grant is bound to this device.
pub(crate) async fn guest_call(
    t: &dyn pai_sync::SyncTransport,
    cx: &GuestCtx<'_>,
    cap_path: &str,
    on: &str,
    want_app: &str,
    op: &str,
    args: &[String],
) -> Result<(DeviceId, Vec<u8>)> {
    let json = std::fs::read_to_string(cap_path)
        .map_err(|e| Error::InvalidInput(format!("{cap_path}: {e}")))?;
    let capability = pai_share::Capability::from_json(&json)
        .map_err(|e| Error::InvalidInput(format!("bad token: {e}")))?;
    if capability.app_id != want_app {
        return Err(Error::InvalidInput(format!(
            "token grants app {} not {want_app}",
            capability.app_id
        )));
    }
    // Guests can't read sealed bcap announcements — a literal device
    // id, a paired prefix, or the token's issuer (`any`) name the host.
    // For a delegated token the serving host is the ROOT issuer — the
    // child's own `issued_by` is the delegating device.
    let to = if on == "any" {
        let mut root = &capability;
        while let Some(p) = &root.parent {
            root = p;
        }
        root.issued_by
    } else {
        match uuid::Uuid::parse_str(on) {
            Ok(u) => DeviceId(u),
            Err(_) => resolve_peer(cx.store, on)?,
        }
    };
    let signer = |msg: &[u8]| {
        cx.ids
            .sign(cx.device.id, cx.key_dir, msg)
            .map_err(|e| pai_share::ShareError::InvalidInput(e.to_string()))
    };
    let resp = pai_share::guest::call_guest(
        t,
        to,
        capability,
        op,
        args,
        std::time::Duration::from_secs(120),
        Some(&signer),
    )
    .await?;
    Ok((to, resp))
}

pub(crate) fn load_package(path: &str) -> Result<pai_apps::AppPackage> {
    pai_apps::AppPackage::load(std::path::Path::new(path))
        .map_err(|e| Error::InvalidInput(e.to_string()))
}

/// Interactive `prompt [default]: ` reader used by the configure flows —
/// empty input keeps `default`.
pub(crate) fn read_prompt(prompt: &str, default: &str) -> Result<String> {
    print!("{prompt} [{default}]: ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut s = String::new();
    std::io::stdin()
        .read_line(&mut s)
        .map_err(|e| Error::Other(e.to_string()))?;
    let s = s.trim();
    Ok(if s.is_empty() {
        default.to_string()
    } else {
        s.to_string()
    })
}

/// RFC 8628 device-authorization: prints the code + URL, then polls until
/// the grant is authorized or expires (backs off on `slow_down`).
pub(crate) async fn wait_device_grant(
    cfg: &pai_oauth::OAuthConfig,
    grant: &pai_oauth::DeviceGrant,
) -> Result<pai_oauth::TokenSet> {
    println!();
    println!("Go to {}", grant.verification_uri);
    if let Some(u) = &grant.verification_uri_complete {
        println!("  (or directly: {u})");
    }
    println!("and enter code: {}", grant.user_code);
    let mut interval = grant.interval.max(1);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(grant.expires_in.max(120));
    let mut pendings = 0u32;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(Error::InvalidInput(
                "device grant expired — re-run configure".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        match pai_oauth::poll_token_once(cfg, &grant.device_code).await? {
            pai_oauth::Poll::Pending => {
                pendings += 1;
                if pendings == 5 {
                    interval += 5; // back off gently
                }
                print!(".");
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
            pai_oauth::Poll::Granted(t) => return Ok(t),
        }
    }
}

/// Record one audit event — the `event()` + `device` + `detail` + `record`
/// quad that app/sync handlers repeat.
pub(crate) fn record_audit(
    store: &Arc<Store>,
    device: DeviceId,
    kind: AuditKind,
    detail: serde_json::Value,
) -> Result<()> {
    let mut ev = pai_audit::event(kind, AuditOutcome::Ok);
    ev.device = Some(device);
    ev.detail = detail;
    pai_audit::AuditLog::new(store.clone()).record(&ev)
}

/// Direct-invocation context for memory tools run from `pai memories` —
/// the CLI user is the operator, so no approval gate, still audited by
/// the caller's `record_audit`.
pub(crate) fn tool_ctx<'a>(ctx: &'a Ctx) -> pai_tools::ToolContext<'a> {
    pai_tools::ToolContext {
        run: AgentRunId::new(),
        device: ctx.agent.device,
        memory: Some(ctx.memory.as_ref()),
        memory_scope: None,
        documents: Some(ctx.documents.as_ref()),
        email: ctx.email.as_deref(),
        gitlab: ctx.gitlab.as_deref(),
        vision: None,
        notify: None,
        allowed_roots: &[],
        apps: None,
        audio_gen: None,
        media_dir: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_slug_makes_dns_labels() {
        assert_eq!(name_slug("Alice"), "alice");
        assert_eq!(name_slug("Martin C."), "martin-c");
        assert_eq!(name_slug("Alice's phone"), "alice-s-phone");
        assert_eq!(name_slug("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn app_name_resolves_own_namespace() {
        assert_eq!(
            parse_app_name("notes.martin.devices", "martin"),
            Some("notes".into())
        );
        // Port + trailing dot tolerated.
        assert_eq!(
            parse_app_name("com.example.app.martin.devices:8787", "martin"),
            Some("com.example.app".into())
        );
        // Other users' namespace isn't answered here.
        assert_eq!(parse_app_name("notes.bob.devices", "martin"), None);
        assert_eq!(parse_app_name("localhost", "martin"), None);
        assert_eq!(parse_app_name("a.b", "martin"), None);
    }
}
