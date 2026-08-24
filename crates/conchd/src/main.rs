use std::{
    env,
    fs::File,
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use conchd::tcp::{Daemon, TransportMode};
use tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    version::TLS13,
    ClientConfig, RootCertStore, ServerConfig,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conchd: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = default_data_dir();
    let mut tcp: SocketAddr = "127.0.0.1:7421".parse()?;
    let mut http: SocketAddr = "127.0.0.1:7420".parse()?;
    let mut mode = TransportMode::Local;
    let mut mode_selected = false;
    let mut tls_cert = None;
    let mut tls_key = None;
    let mut tls_ca = None;
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
            "--localhost" => {
                if mode_selected && mode != TransportMode::Local {
                    return Err("--localhost conflicts with --mode lan/public".into());
                }
                mode = TransportMode::Local;
                mode_selected = true;
            }
            "--mode" => {
                mode = match arguments.next().as_deref() {
                    Some("local") => TransportMode::Local,
                    Some("lan") => TransportMode::Lan,
                    Some("public") => TransportMode::Public,
                    _ => return Err("--mode must be local, lan, or public".into()),
                };
                mode_selected = true;
            }
            "--tls-cert" => {
                tls_cert = Some(PathBuf::from(
                    arguments.next().ok_or("--tls-cert requires a path")?,
                ));
            }
            "--tls-key" => {
                tls_key = Some(PathBuf::from(
                    arguments.next().ok_or("--tls-key requires a path")?,
                ));
            }
            "--tls-ca" => {
                tls_ca = Some(PathBuf::from(
                    arguments.next().ok_or("--tls-ca requires a path")?,
                ));
            }
            "--advertise" => {
                advertised.push(arguments.next().ok_or("--advertise requires an endpoint")?)
            }
            "--version" | "-V" => {
                println!("conchd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }

    if mode == TransportMode::Local && (!tcp.ip().is_loopback() || !http.ip().is_loopback()) {
        return Err("local mode requires literal loopback --tcp and --http addresses".into());
    }
    if mode == TransportMode::Public && (tls_cert.is_none() || tls_key.is_none()) {
        return Err("public mode requires --tls-cert and --tls-key".into());
    }
    if mode != TransportMode::Public && (tls_cert.is_some() || tls_key.is_some()) {
        return Err("--tls-cert and --tls-key require --mode public".into());
    }

    let daemon = Daemon::open(data_dir)?;
    let client = load_client_tls(tls_ca.as_deref())?;
    if mode == TransportMode::Public {
        let cert = tls_cert.expect("validated TLS certificate");
        let key = tls_key.expect("validated TLS key");
        validate_private_key_mode(&key)?;
        let server = load_server_tls(&cert, &key)?;
        daemon.configure_transport(mode, Some(client))?;
        for endpoint in advertised {
            daemon.advertise(&endpoint)?;
        }
        tokio::try_join!(
            daemon.serve_tls(tcp, Arc::clone(&server)),
            daemon.serve_http_tls(http, server)
        )?;
    } else {
        daemon.configure_transport(mode, Some(client))?;
        for endpoint in advertised {
            daemon.advertise(&endpoint)?;
        }
        tokio::try_join!(daemon.serve(tcp), daemon.serve_http(http))?;
    }
    Ok(())
}

fn print_help() {
    println!(
        "conchd {}\n\
         Floor-controlled room daemon.\n\n\
         Usage: conchd [OPTIONS]\n\n\
         Options:\n\
           --data-dir PATH       State directory [default: ~/.conch]\n\
           --tcp ADDRESS         Swarm/client listener [default: 127.0.0.1:7421]\n\
           --http ADDRESS        HTTP/WebSocket listener [default: 127.0.0.1:7420]\n\
           --mode MODE           local, lan, or public [default: local]\n\
           --localhost           Alias for --mode local\n\
           --tls-cert PATH       Public-mode TLS certificate chain\n\
           --tls-key PATH        Public-mode TLS private key (mode 0600)\n\
           --tls-ca PATH         CA bundle for outbound public-mode peers\n\
           --advertise URL       Advertise an allowed tcp(s) or ws(s) endpoint\n\
           -V, --version         Print version\n\
           -h, --help            Print help",
        env!("CARGO_PKG_VERSION")
    );
}

fn load_server_tls(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ServerConfig>, Box<dyn std::error::Error>> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(cert_path)?))
        .collect::<Result<Vec<CertificateDer<'static>>, _>>()?;
    if certs.is_empty() {
        return Err("TLS certificate file contains no certificates".into());
    }
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(File::open(key_path)?))?
            .ok_or("TLS key file contains no private key")?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn load_client_tls(
    ca_path: Option<&Path>,
) -> Result<Arc<ClientConfig>, Box<dyn std::error::Error>> {
    let mut roots = RootCertStore::empty();
    if let Some(path) = ca_path {
        for cert in rustls_pemfile::certs(&mut BufReader::new(File::open(path)?)) {
            roots.add(cert?)?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        if !native.errors.is_empty() && native.certs.is_empty() {
            return Err(format!("failed to load platform CA roots: {:?}", native.errors).into());
        }
        for cert in native.certs {
            roots.add(cert)?;
        }
    }
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn validate_private_key_mode(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err("TLS private key must be mode 0600 or stricter".into());
        }
    }
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
