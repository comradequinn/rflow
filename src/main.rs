mod ca;
mod interceptor;
mod proxy;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use ca::CertificateAuthority;
use interceptor::{Chain, EditRequest, EditResponse, PrintRequest, PrintResponse};
use proxy::Proxy;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage: rflow [options]

options:
  --port <port>        port to listen on (default 8080)
  --print-req          print requests to stdout
  --print-resp         print responses to stdout
  --edit-req <text>    edit requests whose URL contains <text> before sending
  --edit-resp <text>   edit responses to requests whose URL contains <text>
  --ca                 write the CA certificate to stdout and exit
  -h, --help           show this help

edits open in $EDITOR, or vim if it is unset";

struct Args {
    port: u16,
    print_req: bool,
    print_resp: bool,
    edit_req: Option<String>,
    edit_resp: Option<String>,
    ca: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: 8080,
        print_req: false,
        print_resp: false,
        edit_req: None,
        edit_resp: None,
        ca: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                let value = iter.next().ok_or("--port requires a value")?;
                args.port = value.parse().map_err(|_| format!("invalid port: {value}"))?;
            }
            "--print-req" => args.print_req = true,
            "--print-resp" => args.print_resp = true,
            "--edit-req" => args.edit_req = Some(iter.next().ok_or("--edit-req requires a value")?),
            "--edit-resp" => args.edit_resp = Some(iter.next().ok_or("--edit-resp requires a value")?),
            "--ca" => args.ca = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

/// Where the CA and generated host certificates are stored.
fn data_dir() -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".rflow"))
}

/// Writes a file readable only by the current user, as it may contain secrets.
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(contents.as_ref())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rflow: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Error> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "failed to install TLS crypto provider")?;

    let dir = data_dir()?;
    let ca = CertificateAuthority::load_or_create(&dir)?;
    if args.ca {
        print!("{}", ca.cert_pem());
        return Ok(());
    }

    // Requests pass through in order and responses in reverse, so the edits
    // sit at either end to let the printers see what is actually sent.
    let edit_file = dir.join("edit.http");
    let mut chain = Chain::default();
    if let Some(pattern) = args.edit_req {
        chain.add(EditRequest::new(pattern, edit_file.clone()));
    }
    if args.print_req {
        chain.add(PrintRequest);
    }
    if args.print_resp {
        chain.add(PrintResponse);
    }
    if let Some(pattern) = args.edit_resp {
        chain.add(EditResponse::new(pattern, edit_file));
    }

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));
    Arc::new(Proxy::new(chain, ca)).run(addr).await
}
