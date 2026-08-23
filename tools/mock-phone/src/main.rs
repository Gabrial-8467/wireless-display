mod client;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Context as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("discover") => discover(timeout_secs(&args)).await,
        Some("pair") => {
            let opts = ConnectOpts::parse(&args)?;
            let code = opt_value(&args, "--code")
                .context("pair requires --code <6 digits>")?;
            pair(opts, code).await
        }
        Some("run") => {
            let opts = ConnectOpts::parse(&args)?;
            run(opts).await
        }
        _ => {
            eprintln!(
                "usage: mock-phone discover [--timeout 3]\n\
                 \x20      mock-phone pair --host IP --port N --code CODE --name NAME [--state DIR]\n\
                 \x20      mock-phone run  --host IP --port N --name NAME [--state DIR]"
            );
            std::process::exit(2)
        }
    }
}

fn timeout_secs(args: &[String]) -> u64 {
    opt_value(args, "--timeout")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

fn opt_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

struct ConnectOpts {
    host: IpAddr,
    port: u16,
    name: String,
    state_dir: std::path::PathBuf,
}

impl ConnectOpts {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let host: IpAddr = opt_value(args, "--host")
            .context("--host IP is required")?
            .parse()
            .context("invalid --host")?;
        let port: u16 = opt_value(args, "--port")
            .context("--port is required")?
            .parse()
            .context("invalid --port")?;
        let name = opt_value(args, "--name").unwrap_or_else(|| "Mock Phone".into());
        let state_dir = opt_value(args, "--state").map_or_else(
            || {
                dirs::data_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("wireless-display-mock")
            },
            std::path::PathBuf::from,
        );
        Ok(Self {
            host,
            port,
            name,
            state_dir,
        })
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredDevice {
    device_id: String,
    token: String,
}

fn store_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("device.json")
}

async fn discover(timeout: u64) -> anyhow::Result<()> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let receiver =
        daemon.browse(client::SERVICE_TYPE).context("failed to start mDNS browse")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut found = 0;
    while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match tokio::time::timeout(remaining, async { receiver.recv() }).await {
            Err(_) => break,
            Ok(Err(_)) => break,
            Ok(Ok(event)) => {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                    let v4set = info.get_addresses_v4();
                    let Some(v4) = v4set.iter().next() else {
                        continue;
                    };
                    let ip = IpAddr::from(*v4);
                    println!(
                        "{}\t{}:{}\tfp={}\tproto={}",
                        info.get_fullname(),
                        ip,
                        info.get_port(),
                        info.get_property_val_str("fp").unwrap_or("?"),
                        info.get_property_val_str("proto").unwrap_or("?"),
                    );
                    found += 1;
                }
            }
        }
    }
    daemon.stop_browse(client::SERVICE_TYPE).ok();
    if found == 0 {
        anyhow::bail!("no receivers advertised within {timeout}s");
    }
    Ok(())
}

async fn pair(opts: ConnectOpts, code: String) -> anyhow::Result<()> {
    let conn = client::connect(opts.addr()).await?;
    let outcome = client::pair_with_receiver(&conn, &opts.name, &code).await?;
    println!("paired: device_id={} token stored", outcome.device_id);
    std::fs::create_dir_all(&opts.state_dir)?;
    let path = store_path(&opts.state_dir);
    let stored = StoredDevice {
        device_id: outcome.device_id,
        token: outcome.device_token,
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)?;
    println!("saved credentials to {}", path.display());
    Ok(())
}

async fn run(opts: ConnectOpts) -> anyhow::Result<()> {
    let raw = std::fs::read(store_path(&opts.state_dir))
        .context("no stored pairing; run `mock-phone pair` first")?;
    let stored: StoredDevice = serde_json::from_slice(&raw)?;

    let conn = client::connect(opts.addr()).await?;
    let receiver_name = client::authenticate(&conn, &opts.name, &stored.token).await?;
    println!("connected to “{receiver_name}”");
    client::serve_until_bye(&conn).await?;
    println!("receiver closed the session");
    Ok(())
}
