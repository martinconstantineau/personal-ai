//! `pai mesh …` — LAN discovery for paired devices.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum MeshCmd {
    /// Listen for signed LAN announcements and list the paired devices
    /// serving a sync relay right now.
    Discover {
        /// Seconds to listen.
        #[arg(long, default_value = "3")]
        timeout_secs: u64,
    },
}

pub(crate) async fn run(cmd: &MeshCmd, b: &Base) -> Result<()> {
    let store = b.store.clone();
    let ids = b.ids();
    match cmd {
        MeshCmd::Discover { timeout_secs } => {
            let bind = std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                pai_mesh::MULTICAST_PORT,
            );
            let sock = pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP))?;
            let found = pai_mesh::discover(&sock, std::time::Duration::from_secs(*timeout_secs));
            let paired = pai_mesh::paired_announcements(&store, &ids, found.clone())?;
            for p in &paired {
                println!(
                    "  {}  {:<20} {:<10} relay http://{}",
                    p.peer.device_id,
                    clean(&p.peer.name),
                    clean(&p.peer.platform),
                    p.relay_addr
                );
            }
            println!(
                "{} paired device(s) announcing ({} datagram(s) ignored)",
                paired.len(),
                found.len() - paired.len()
            );
            if !paired.is_empty() {
                println!("sync now: `pai sync run --lan`");
            }
        }
    }
    Ok(())
}
