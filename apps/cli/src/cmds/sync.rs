//! `pai sync …` — cross-device sync over a shared folder or relay:
//! push/pull/run, vault-key rotation, status, and the relay server.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum SyncCmd {
    /// Seal + push local changes to a shared folder or relay.
    Push {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        /// Bearer token for the relay (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Pull + apply remote changes from a shared folder or relay.
    Pull {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Push then pull in one pass.
    Run {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Rotate the vault key and push sealed rotation objects to every
    /// paired peer — they adopt on their next sync pull/run. Use after
    /// `pair remove` to actually revoke a device's access.
    Rotate {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Peers + object count at the destination.
    Status {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Run a sync relay server — stores ciphertext objects under --dir.
    /// Put it behind TLS (reverse proxy) off localhost; the blobs are
    /// sealed anyway, but auth keeps it from being a free object store.
    Serve {
        #[arg(long)]
        dir: String,
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Require `Authorization: Bearer <token>` (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// Mesh mode: broadcast a signed LAN announcement and accept
        /// peer-key bearer tokens (no --token needed). Bind 0.0.0.0 to
        /// serve other devices on the LAN.
        #[arg(long)]
        announce: bool,
    },
}

pub(crate) async fn run(cmd: &SyncCmd, b: &Base) -> Result<()> {
    use pai_sync::{crypto, engine, pair, SyncTransport};
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let ids = b.ids();
    let key_dir = b.key_dir.clone();
    let device = b.device.clone();
    match cmd {
        SyncCmd::Push {
            dir,
            relay,
            token,
            lan,
            to,
        }
        | SyncCmd::Pull {
            dir,
            relay,
            token,
            lan,
            to,
        }
        | SyncCmd::Run {
            dir,
            relay,
            token,
            lan,
            to,
        } => {
            let t = if *lan {
                lan_transport(to, &store, &ids, &device, &cfg.data_dir)?
            } else {
                sync_transport(dir, relay, token)?
            };
            let kind = t.id().to_string();
            // Adopt any pending vault rotation first — objects sealed
            // under the new vault need the new key before pull.
            let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
            let adopted = pai_sync::rotate::adopt_rotations(
                &*t,
                &store,
                &cfg.data_dir,
                &agree,
                &device,
                &ids,
                &key_dir,
            )
            .await?;
            if adopted > 0 {
                println!(
                    "adopted vault rotation (epoch {})",
                    pai_sync::rotate::vault_epoch(&cfg.data_dir)
                );
            }
            let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                Error::Sync("no vault key — pair a device first (pai pair)".into())
            })?;
            let eng = engine::SyncEngine::new(t, store.clone(), vault, device.id, &cfg.data_dir);
            let out = match cmd {
                SyncCmd::Push { .. } => eng.push().await?,
                SyncCmd::Pull { .. } => eng.pull().await?,
                _ => eng.run().await?,
            };
            println!(
                "sync via {kind}: pushed {}, pulled {}, skipped {}",
                out.pushed, out.pulled, out.skipped
            );
        }
        SyncCmd::Rotate { dir, relay, token } => {
            let t = sync_transport(dir, relay, token)?;
            let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
            let n = pai_sync::rotate::push_rotation(
                &*t,
                &store,
                &cfg.data_dir,
                &device,
                &agree,
                &ids,
                &key_dir,
            )
            .await?;
            println!(
                "vault rotated (epoch {}) — notified {n} peer(s); \
                     they adopt on their next sync pull/run",
                pai_sync::rotate::vault_epoch(&cfg.data_dir)
            );
        }
        SyncCmd::Status {
            dir,
            relay,
            token,
            lan,
            to,
        } => {
            let peers = pair::list_peers(&store)?;
            println!("{} paired device(s)", peers.len());
            if dir.is_some() || relay.is_some() || *lan {
                let t = if *lan {
                    lan_transport(to, &store, &ids, &device, &cfg.data_dir)?
                } else {
                    sync_transport(dir, relay, token)?
                };
                let metas = t.list().await?;
                let tombstones = metas.iter().filter(|m| m.tombstone).count();
                println!(
                    "{}: {} object(s) ({} tombstone(s))",
                    t.id(),
                    metas.len(),
                    tombstones
                );
            }
        }
        SyncCmd::Serve {
            dir,
            addr,
            token,
            announce,
        } => {
            if *announce {
                // Mesh mode: peer-key bearer auth + signed multicast
                // announcement. No token file — only paired devices
                // can compute hex(peer_key).
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let st = store.clone();
                let tokens = std::sync::Arc::new(move || {
                    pai_mesh::relay_tokens(&st, &agree.secret).unwrap_or_default()
                });
                let srv = pai_sync::relay::bind_dynamic(dir.into(), addr, tokens)?;
                let port: u16 = srv
                    .addr()
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(|| Error::Sync("bad relay addr".into()))?;
                {
                    let (st2, dev2, kd2) = (store.clone(), device.clone(), key_dir.clone());
                    std::thread::spawn(move || {
                        let ids2 = pai_identity::IdentityStore::new(st2);
                        let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
                            return;
                        };
                        let _ = sock.set_multicast_loop_v4(true);
                        let dest = std::net::SocketAddr::new(
                            std::net::IpAddr::V4(pai_mesh::MULTICAST_GROUP),
                            pai_mesh::MULTICAST_PORT,
                        );
                        loop {
                            if let Ok(a) = pai_mesh::make_announcement(&ids2, &dev2, &kd2, port) {
                                let _ = pai_mesh::send_announcement(&sock, &a, dest);
                            }
                            std::thread::sleep(std::time::Duration::from_secs(2));
                        }
                    });
                }
                println!(
                    "mesh relay on http://{} storing under {dir} \
                         — announcing to paired devices (peer-key auth)",
                    srv.addr()
                );
                pai_sync::relay::serve(srv);
            } else {
                let token = token
                    .clone()
                    .or_else(|| std::env::var("PAI_SYNC_TOKEN").ok());
                let srv = pai_sync::relay::bind(dir.into(), addr, token.clone())?;
                println!(
                    "relay on http://{} storing under {dir} {}",
                    srv.addr(),
                    if token.is_some() {
                        "(token required)"
                    } else {
                        "(NO AUTH — localhost use only)"
                    }
                );
                pai_sync::relay::serve(srv);
            }
        }
    }
    Ok(())
}
