use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use cfp_protocol::*;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use rustls::ServerConfig;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, mpsc},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "server.yaml")]
    config: String,
}
#[derive(Clone, Deserialize)]
struct Route {
    listen: String,
    target: String,
}
#[derive(Clone, Deserialize)]
struct Client {
    token_sha256: String,
    routes: Vec<Route>,
}
#[derive(Deserialize)]
struct Config {
    listen: String,
    #[serde(default = "default_http_listen")]
    http_listen: String,
    cert: Option<String>,
    key: Option<String>,
    clients: Vec<Client>,
}
type Streams = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;
type DomainRoutes = Arc<RwLock<HashMap<String, DomainRoute>>>;

#[derive(Clone)]
struct DomainRoute {
    session_id: u64,
    target: String,
    out: mpsc::Sender<Frame>,
    streams: Streams,
}

fn default_http_listen() -> String {
    "127.0.0.1:445".into()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg: Config = serde_yaml::from_reader(File::open(Args::parse().config)?)?;
    validate_config(&cfg)?;
    let acceptor = match (&cfg.cert, &cfg.key) {
        (Some(cert), Some(key)) => Some(TlsAcceptor::from(Arc::new(tls_config(cert, key)?))),
        (None, None) => None,
        _ => anyhow::bail!("cert e key devem ser configurados juntos"),
    };
    let listener = TcpListener::bind(&cfg.listen).await?;
    info!(listen = %cfg.listen, tls = acceptor.is_some(), "servidor iniciado");
    let clients = Arc::new(cfg.clients);
    let domains: DomainRoutes = Default::default();
    let http_listener = TcpListener::bind(&cfg.http_listen).await?;
    info!(listen = %cfg.http_listen, "roteador HTTP por dominio iniciado");
    tokio::spawn(serve_http(http_listener, domains.clone()));
    loop {
        let (tcp, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let clients = clients.clone();
        let domains = domains.clone();
        tokio::spawn(async move {
            let result = match acceptor {
                Some(acceptor) => {
                    async {
                        let tls = acceptor.accept(tcp).await?;
                        let ws = accept_async(tls).await?;
                        serve(ws, clients, domains).await
                    }
                    .await
                }
                None => {
                    async {
                        let ws = accept_async(tcp).await?;
                        serve(ws, clients, domains).await
                    }
                    .await
                }
            };
            if let Err(error) = result {
                warn!(%peer, %error, "sessao encerrada");
            }
        });
    }
}

async fn serve<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    clients: Arc<Vec<Client>>,
    domains: DomainRoutes,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = ws.split();
    let auth = reader.next().await.context("AUTH ausente")??;
    let auth = Frame::decode(&auth.into_data())?;
    if auth.kind != AUTH {
        anyhow::bail!("primeiro frame nao e AUTH");
    }
    let digest = hex::encode(Sha256::digest(&auth.payload));
    let client = clients
        .iter()
        .find(|c| c.token_sha256.eq_ignore_ascii_case(&digest))
        .cloned();
    let Some(client) = client else {
        writer
            .send(Message::Binary(
                Frame::new(
                    ERROR,
                    0,
                    "token invalido: server.yaml deve conter o SHA-256 do token",
                )
                .encode()
                .into(),
            ))
            .await?;
        writer.close().await?;
        anyhow::bail!("token invalido");
    };
    writer
        .send(Message::Binary(Frame::new(AUTH_OK, 0, []).encode().into()))
        .await?;

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let streams: Streams = Default::default();
    static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut route_tasks = Vec::new();
    for route in client.routes {
        if route.listen.parse::<std::net::SocketAddr>().is_ok() {
            let tx = out_tx.clone();
            let streams = streams.clone();
            route_tasks.push(tokio::spawn(async move {
                if let Err(e) = route_listener(route, tx, streams).await {
                    error!(%e, "rota parou");
                }
            }));
        } else {
            let hostname = normalize_hostname(&route.listen);
            domains.write().await.insert(
                hostname.clone(),
                DomainRoute {
                    session_id,
                    target: route.target,
                    out: out_tx.clone(),
                    streams: streams.clone(),
                },
            );
            info!(%hostname, "rota de dominio ativa");
        }
    }
    let write_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            writer.send(Message::Binary(frame.encode().into())).await?;
        }
        anyhow::Ok(())
    });
    while let Some(message) = reader.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "erro na sessao WebSocket");
                break;
            }
        };
        if !message.is_binary() {
            continue;
        }
        let frame = Frame::decode(&message.into_data())?;
        match frame.kind {
            DATA => {
                if let Some(tx) = streams.lock().await.get(&frame.stream_id).cloned() {
                    let _ = tx.send(frame.payload).await;
                }
            }
            CLOSE | OPEN_ERROR => {
                streams.lock().await.remove(&frame.stream_id);
            }
            _ => {}
        }
    }
    for task in route_tasks {
        task.abort();
    }
    domains
        .write()
        .await
        .retain(|_, route| route.session_id != session_id);
    write_task.abort();
    Ok(())
}

async fn route_listener(route: Route, out: mpsc::Sender<Frame>, streams: Streams) -> Result<()> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let listener = TcpListener::bind(&route.listen).await?;
    info!(listen = %route.listen, target = %route.target, "rota ativa");
    loop {
        let (stream, _) = listener.accept().await?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        bridge_public(
            stream,
            id,
            route.target.clone(),
            out.clone(),
            streams.clone(),
            Vec::new(),
        )
        .await;
    }
}

async fn bridge_public(
    stream: TcpStream,
    id: u32,
    target: String,
    out: mpsc::Sender<Frame>,
    streams: Streams,
    initial_data: Vec<u8>,
) {
    let (mut read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    streams.lock().await.insert(id, tx);
    if out.send(Frame::new(OPEN, id, target)).await.is_err() {
        return;
    }
    if !initial_data.is_empty() && out.send(Frame::new(DATA, id, initial_data)).await.is_err() {
        return;
    }
    let out_read = out.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0; 16 * 1024];
        loop {
            match read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_read
                        .send(Frame::new(DATA, id, &buffer[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = out_read.send(Frame::new(CLOSE, id, [])).await;
    });
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if write.write_all(&data).await.is_err() {
                break;
            }
        }
    });
}

async fn serve_http(listener: TcpListener, domains: DomainRoutes) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let domains = domains.clone();
                tokio::spawn(async move {
                    if let Err(error) = route_http(stream, domains).await {
                        warn!(%peer, %error, "requisicao HTTP recusada");
                    }
                });
            }
            Err(error) => error!(%error, "falha no listener HTTP"),
        }
    }
}

async fn route_http(mut stream: TcpStream, domains: DomainRoutes) -> Result<()> {
    const MAX_HEADER: usize = 32 * 1024;
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("conexao fechada antes dos headers");
        }
        request.extend_from_slice(&chunk[..n]);
        if request.len() > MAX_HEADER {
            stream
                .write_all(
                    b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n",
                )
                .await?;
            anyhow::bail!("headers maiores que {MAX_HEADER} bytes");
        }
    }

    let hostname = parse_host(&request).context("header Host ausente ou invalido")?;
    let route = domains.read().await.get(&hostname).cloned();
    let Some(route) = route else {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .await?;
        anyhow::bail!("dominio sem rota: {hostname}");
    };
    static NEXT_ID: AtomicU32 = AtomicU32::new(1_000_000);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    bridge_public(stream, id, route.target, route.out, route.streams, request).await;
    Ok(())
}

fn parse_host(request: &[u8]) -> Option<String> {
    let headers = std::str::from_utf8(request).ok()?;
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| normalize_hostname(value.trim()))
    })
}

fn normalize_hostname(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .split(':')
        .next()
        .unwrap_or(host)
        .to_ascii_lowercase()
}

fn validate_config(config: &Config) -> Result<()> {
    let mut tokens = HashSet::new();
    let mut listeners = HashSet::new();
    for client in &config.clients {
        if client.token_sha256.len() != 64
            || !client
                .token_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("token_sha256 deve ter exatamente 64 caracteres hexadecimais");
        }
        if !tokens.insert(client.token_sha256.to_ascii_lowercase()) {
            anyhow::bail!("token_sha256 duplicado no server.yaml");
        }
        for route in &client.routes {
            let listener = if route.listen.parse::<std::net::SocketAddr>().is_ok() {
                route.listen.clone()
            } else {
                normalize_hostname(&route.listen)
            };
            if listener.is_empty() {
                anyhow::bail!("listen vazio em uma rota");
            }
            if !listeners.insert(listener.clone()) {
                anyhow::bail!("listen duplicado no server.yaml: {listener}");
            }
        }
    }
    Ok(())
}

fn tls_config(cert: &str, key: &str) -> Result<ServerConfig> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(cert)?))
        .collect::<std::io::Result<Vec<_>>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(key)?))?
        .context("chave privada ausente")?;
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_normalizes_http_host() {
        let request = b"GET / HTTP/1.1\r\nhOsT: B.Rotava.com:443\r\n\r\n";
        assert_eq!(parse_host(request).as_deref(), Some("b.rotava.com"));
    }

    #[test]
    fn rejects_request_without_host() {
        assert!(parse_host(b"GET / HTTP/1.0\r\n\r\n").is_none());
    }

    #[test]
    fn rejects_duplicate_tokens_and_routes() {
        let client = |token: &str, listen: &str| Client {
            token_sha256: token.into(),
            routes: vec![Route {
                listen: listen.into(),
                target: "127.0.0.1:1".into(),
            }],
        };
        let token = "a".repeat(64);
        let mut config = Config {
            listen: "127.0.0.1:444".into(),
            http_listen: "127.0.0.1:445".into(),
            cert: None,
            key: None,
            clients: vec![
                client(&token, "b.rotava.com"),
                client(&token, "c.rotava.com"),
            ],
        };
        assert!(validate_config(&config).is_err());

        config.clients[1].token_sha256 = "b".repeat(64);
        config.clients[1].routes[0].listen = "B.ROTAVA.COM".into();
        assert!(validate_config(&config).is_err());
    }
}
