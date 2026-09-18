//! `pai pair …` — device pairing for end-to-end encrypted sync:
//! offer/accept/complete, QR payloads, fingerprints, peer list/remove.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum PairCmd {
    /// Write a signed pairing offer for another device to accept.
    Offer {
        #[arg(long)]
        out: String,
        /// Also print the offer as a QR code — the other device scans
        /// it instead of receiving the file.
        #[arg(long)]
        qr: bool,
    },
    /// Accept an offer file; writes the signed accept (carries the vault
    /// key sealed to the offerer).
    Accept {
        offer: String,
        #[arg(long)]
        out: String,
        /// Also print the accept as a QR code — the offering device
        /// scans it instead of receiving the file back.
        #[arg(long)]
        qr: bool,
    },
    /// Complete pairing from an accept file; installs the vault key.
    Complete { accept: String },
    /// Print a pairing file's key fingerprint — compare it against the
    /// other device over a channel the file didn't travel (voice call,
    /// in person) before accepting. Matching fingerprints prove both
    /// ends hold the same key; the signature alone can't.
    Verify { file: String },
    /// List trusted peer devices.
    List,
    /// Remove a peer. Note: does NOT rotate the vault key — a removed
    /// peer may still hold it.
    Remove { id: String },
}

pub(crate) async fn run(cmd: &PairCmd, b: &Base) -> Result<()> {
    use pai_sync::{crypto, pair};
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let ids = b.ids();
    let key_dir = b.key_dir.clone();
    let device = b.device.clone();
    match cmd {
        PairCmd::Offer { out, qr } => {
            let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
            let m = pair::make_offer(&device, &agree, &ids, &key_dir)?;
            pair::write_message(&m, std::path::Path::new(out))?;
            println!("offer for '{}' written to {out}", clean(&device.name));
            println!(
                "fingerprint: {} — verify it matches on the other \
                     device (`pai pair verify {out}`)",
                pair::fingerprint(&device.public_key)
            );
            if *qr {
                let payload = serde_json::to_string(&m).map_err(|e| Error::Sync(e.to_string()))?;
                print_qr(&payload)?;
                println!("scan me with the other device's camera");
            } else {
                println!("send it to the other device: pai pair accept {out} --out accept.pai");
            }
        }
        PairCmd::Accept { offer, out, qr } => {
            let offer = pair::read_message(std::path::Path::new(offer))?;
            let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
            let m = pair::accept_offer(
                &store,
                &offer,
                &device,
                &agree,
                &ids,
                &key_dir,
                &cfg.data_dir,
            )?;
            pair::write_message(&m, std::path::Path::new(out))?;
            println!(
                "paired with '{}' ({}…); accept written to {out}",
                clean(&offer.name),
                &offer.device_id[..8.min(offer.device_id.len())]
            );
            if *qr {
                let payload = serde_json::to_string(&m).map_err(|e| Error::Sync(e.to_string()))?;
                print_qr(&payload)?;
                println!("scan me with the offering device");
            } else {
                println!("return it to the offering device: pai pair complete {out}");
            }
        }
        PairCmd::Complete { accept } => {
            let accept = pair::read_message(std::path::Path::new(accept))?;
            let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
            pair::complete_pairing(&store, &accept, &agree, &cfg.data_dir)?;
            println!(
                "paired with '{}' — vault key installed",
                clean(&accept.name)
            );
        }
        PairCmd::Verify { file } => {
            let m = pair::read_message(std::path::Path::new(file))?;
            m.verify()?;
            let peer = m.peer()?;
            println!(
                "{} {} — key fingerprint {}",
                m.kind,
                clean(&m.name),
                pair::fingerprint(&peer.ed_pubkey)
            );
        }
        PairCmd::List => {
            let peers = pair::list_peers(&store)?;
            if peers.is_empty() {
                println!("no paired devices — see `pai pair offer`");
            }
            for p in peers {
                println!(
                    "  {}  {:<16} {:<10} paired {}",
                    &p.device_id.to_string()[..8],
                    clean(&p.name),
                    clean(&p.platform),
                    p.paired_at.format("%Y-%m-%d %H:%M")
                );
            }
        }
        PairCmd::Remove { id } => {
            if pair::remove_peer(&store, DeviceId(parse_uuid(id, "device")?))? {
                println!(
                    "removed peer {id} — run `pai sync rotate` to revoke \
                         their access (they keep a stale vault key until then)"
                );
            } else {
                println!("no such peer: {id}");
            }
        }
    }
    Ok(())
}
