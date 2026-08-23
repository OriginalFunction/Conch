use std::{env, net::SocketAddr, path::PathBuf};

use conchd::tcp::Daemon;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conchd: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = default_data_dir();
    let mut tcp: SocketAddr = "0.0.0.0:7421".parse()?;
    let mut localhost = false;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => {
                data_dir = PathBuf::from(arguments.next().ok_or("--data-dir requires a path")?);
            }
            "--tcp" => {
                tcp = arguments
                    .next()
                    .ok_or("--tcp requires an address")?
                    .parse()?;
            }
            "--localhost" => localhost = true,
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }

    if localhost {
        tcp.set_ip("127.0.0.1".parse()?);
    }

    Daemon::open(data_dir)?.serve(tcp).await?;
    Ok(())
}

fn default_data_dir() -> PathBuf {
    env::var_os("CONCH_DATA_DIR").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".conch")
        },
        PathBuf::from,
    )
}
