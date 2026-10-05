//! Test seeder for `tests/e2e.py`: makes a torrent of a directory,
//! announced to one tracker, writes the `.torrent` and seeds it until its
//! stdin closes. No DHT, no local discovery: peers only meet through the
//! tracker.
//!
//! `seed <dir> <tracker URL> <out.torrent>`

use std::path::PathBuf;

use librqbit::{CreateTorrentOptions, ListenerOptions, Session, SessionOptions};
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, tracker, out] = args.as_slice() else {
        anyhow::bail!("usage: seed <dir> <tracker URL> <out.torrent>");
    };
    let dir = PathBuf::from(dir);
    let parent = dir.parent().unwrap_or(&dir).to_path_buf();
    let opts = SessionOptions {
        dht: None,
        disable_local_service_discovery: true,
        listen: Some(ListenerOptions { listen_addr: "127.0.0.1:0".parse()?, ..Default::default() }),
        ..Default::default()
    };
    let session = Session::new_with_opts(parent, opts).await?;
    let (torrent, _handle) = session
        .create_and_serve_torrent(
            &dir,
            CreateTorrentOptions { name: None, trackers: vec![tracker.clone()], piece_length: Some(256 * 1024) },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(out, torrent.as_bytes()?)?;
    println!("seeding {}", torrent.info_hash().as_string());
    let mut sink = Vec::new();
    let _ = tokio::io::stdin().read_to_end(&mut sink).await;
    Ok(())
}
