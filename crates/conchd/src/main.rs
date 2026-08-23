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
    let mut http: SocketAddr = "0.0.0.0:7420".parse()?;
    let mut localhost = false;
    let mut advertised = Vec::new();
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
            "--http" => {
                http = arguments
                    .next()
                    .ok_or("--http requires an address")?
                    .parse()?;
            }
            "--localhost" => localhost = true,
            "--advertise" => {
                advertised.push(arguments.next().ok_or("--advertise requires an endpoint")?)
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }

    if localhost {
        tcp.set_ip("127.0.0.1".parse()?);
        http.set_ip("127.0.0.1".parse()?);
    }

    let daemon = Daemon::open(data_dir)?;
    for endpoint in advertised {
        daemon.advertise(&endpoint)?;
    }
    tokio::try_join!(daemon.serve(tcp), daemon.serve_http(http))?;
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
