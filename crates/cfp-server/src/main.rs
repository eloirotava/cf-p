use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use cfp_protocol::*;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use rustls::ServerConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, mpsc, watch},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "server.yaml")]
    config: String,
}
#[derive(Clone, Deserialize, Serialize)]
struct Route {
    listen: String,
    target: String,
}
#[derive(Clone, Deserialize, Serialize)]
struct Client {
    token_sha256: String,
    routes: Vec<Route>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Config {
    listen: String,
    #[serde(default = "default_http_listen")]
    http_listen: String,
    cert: Option<String>,
    key: Option<String>,
    admin: Option<AdminConfig>,
    clients: Vec<Client>,
}
#[derive(Clone, Deserialize, Serialize)]
struct AdminConfig {
    #[serde(default = "default_admin_listen")]
    listen: String,
    username: String,
    password: String,
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
fn default_admin_listen() -> String {
    "127.0.0.1:446".into()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let config_path = PathBuf::from(Args::parse().config);
    let cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
    validate_config(&cfg)?;
    let acceptor = match (&cfg.cert, &cfg.key) {
        (Some(cert), Some(key)) => Some(TlsAcceptor::from(Arc::new(tls_config(cert, key)?))),
        (None, None) => None,
        _ => anyhow::bail!("cert e key devem ser configurados juntos"),
    };
    let listener = TcpListener::bind(&cfg.listen).await?;
    info!(listen = %cfg.listen, tls = acceptor.is_some(), "servidor iniciado");
    let clients = Arc::new(RwLock::new(cfg.clients.clone()));
    let (reload_tx, reload_rx) = watch::channel(0_u64);
    if let Some(admin) = cfg.admin.clone() {
        let listener = TcpListener::bind(&admin.listen).await?;
        info!(listen = %admin.listen, "painel administrativo iniciado");
        tokio::spawn(serve_admin(
            listener,
            admin,
            config_path,
            clients.clone(),
            reload_tx,
        ));
    }
    let domains: DomainRoutes = Default::default();
    let http_listener = TcpListener::bind(&cfg.http_listen).await?;
    info!(listen = %cfg.http_listen, "roteador HTTP por dominio iniciado");
    tokio::spawn(serve_http(http_listener, domains.clone()));
    loop {
        let (tcp, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let clients = clients.clone();
        let reload = reload_rx.clone();
        let domains = domains.clone();
        tokio::spawn(async move {
            let result = match acceptor {
                Some(acceptor) => {
                    async {
                        let tls = acceptor.accept(tcp).await?;
                        let ws = accept_async(tls).await?;
                        serve(ws, clients, domains, reload).await
                    }
                    .await
                }
                None => {
                    async {
                        let ws = accept_async(tcp).await?;
                        serve(ws, clients, domains, reload).await
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
    clients: Arc<RwLock<Vec<Client>>>,
    domains: DomainRoutes,
    mut reload: watch::Receiver<u64>,
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
        .read()
        .await
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
    loop {
        let message = tokio::select! {
            message = reader.next() => message,
            changed = reload.changed() => {
                if changed.is_ok() { info!(session_id, "configuracao alterada; reconectando cliente"); }
                break;
            }
        };
        let Some(message) = message else { break };
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

async fn serve_admin(
    listener: TcpListener,
    admin: AdminConfig,
    config_path: PathBuf,
    clients: Arc<RwLock<Vec<Client>>>,
    reload: watch::Sender<u64>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let (admin, path, clients, reload) = (
                    admin.clone(),
                    config_path.clone(),
                    clients.clone(),
                    reload.clone(),
                );
                tokio::spawn(async move {
                    if let Err(error) = handle_admin(stream, admin, path, clients, reload).await {
                        warn!(%peer, %error, "requisicao administrativa recusada");
                    }
                });
            }
            Err(error) => error!(%error, "falha no listener administrativo"),
        }
    }
}

async fn handle_admin(
    mut stream: TcpStream,
    admin: AdminConfig,
    config_path: PathBuf,
    clients: Arc<RwLock<Vec<Client>>>,
    reload: watch::Sender<u64>,
) -> Result<()> {
    let request = read_http_request(&mut stream, 64 * 1024).await?;
    if !basic_auth_ok(&request, &admin) {
        return http_response(
            &mut stream,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            "Autenticacao necessaria",
            &["WWW-Authenticate: Basic realm=\"cf-p\""],
        )
        .await;
    }
    let header_end = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("headers invalidos")?
        + 4;
    let head = std::str::from_utf8(&request[..header_end])?;
    let first = head.lines().next().context("linha HTTP ausente")?;
    let mut pieces = first.split_whitespace();
    let method = pieces.next().unwrap_or("");
    let path = pieces.next().unwrap_or("");

    if method == "POST" {
        if !same_origin_or_absent(head) {
            return http_response(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                "Origem recusada",
                &[],
            )
            .await;
        }
        let form = parse_form(&request[header_end..]);
        let mut cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
        let mut issued_token = None;
        match path {
            "/clients" => {
                let mut bytes = [0_u8; 32];
                rand::rng().fill_bytes(&mut bytes);
                let token = hex::encode(bytes);
                cfg.clients.push(Client {
                    token_sha256: hex::encode(Sha256::digest(token.as_bytes())),
                    routes: vec![],
                });
                issued_token = Some(token);
            }
            "/routes" => {
                let index: usize = field(&form, "client")?.parse()?;
                let client = cfg.clients.get_mut(index).context("cliente inexistente")?;
                client.routes.push(Route {
                    listen: field(&form, "listen")?.into(),
                    target: field(&form, "target")?.into(),
                });
            }
            "/delete-route" => {
                let client: usize = field(&form, "client")?.parse()?;
                let route: usize = field(&form, "route")?.parse()?;
                cfg.clients
                    .get_mut(client)
                    .context("cliente inexistente")?
                    .routes
                    .get(route)
                    .context("rota inexistente")?;
                cfg.clients[client].routes.remove(route);
            }
            "/delete-client" => {
                let index: usize = field(&form, "client")?.parse()?;
                cfg.clients.get(index).context("cliente inexistente")?;
                cfg.clients.remove(index);
            }
            _ => {
                return http_response(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    "Nao encontrado",
                    &[],
                )
                .await;
            }
        }
        validate_config(&cfg)?;
        save_config(&config_path, &cfg)?;
        *clients.write().await = cfg.clients.clone();
        reload.send_modify(|generation| *generation += 1);
        let body = render_admin(&cfg, issued_token.as_deref());
        return http_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            &body,
            &[],
        )
        .await;
    }
    let cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
    http_response(
        &mut stream,
        "200 OK",
        "text/html; charset=utf-8",
        &render_admin(&cfg, None),
        &[],
    )
    .await
}

async fn read_http_request(stream: &mut TcpStream, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buf = [0_u8; 2048];
    let mut wanted = None;
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() > limit {
            anyhow::bail!("requisicao administrativa grande demais");
        }
        if wanted.is_none() {
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = std::str::from_utf8(&data[..end + 4])?;
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                wanted = Some(end + 4 + length);
            }
        }
        if wanted.is_some_and(|length| data.len() >= length) {
            break;
        }
    }
    Ok(data)
}

fn basic_auth_ok(request: &[u8], admin: &AdminConfig) -> bool {
    let Ok(head) = std::str::from_utf8(request) else {
        return false;
    };
    let expected = BASE64.encode(format!("{}:{}", admin.username, admin.password));
    head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("authorization")
                && value.trim() == format!("Basic {expected}")
        })
    })
}

fn same_origin_or_absent(headers: &str) -> bool {
    let get = |wanted: &str| {
        headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted).then(|| value.trim())
        })
    };
    match (get("origin"), get("host")) {
        (None, _) => true,
        (Some(origin), Some(host)) => {
            origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                == Some(host)
        }
        _ => false,
    }
}

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect()
}
fn url_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
fn field<'a>(form: &'a HashMap<String, String>, name: &str) -> Result<&'a str> {
    form.get(name)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("campo {name} ausente"))
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_admin(cfg: &Config, token: Option<&str>) -> String {
    let mut cards = String::new();
    for (ci, client) in cfg.clients.iter().enumerate() {
        let short = &client.token_sha256[..client.token_sha256.len().min(12)];
        cards.push_str(&format!("<section><h2>Cliente {} <code>{}…</code></h2><form method=post action=/delete-client><input type=hidden name=client value={ci}><button class=danger>Excluir cliente</button></form>", ci + 1, escape(short)));
        for (ri, route) in client.routes.iter().enumerate() {
            cards.push_str(&format!("<div class=route><code>{}</code> → <code>{}</code><form method=post action=/delete-route><input type=hidden name=client value={ci}><input type=hidden name=route value={ri}><button class=danger>Excluir</button></form></div>", escape(&route.listen), escape(&route.target)));
        }
        cards.push_str(&format!("<form method=post action=/routes><input type=hidden name=client value={ci}><input name=listen required placeholder='b.rotava.com ou 0.0.0.0:33890'><input name=target required placeholder='127.0.0.1:3000'><button>Adicionar rota</button></form></section>"));
    }
    let notice = token.map(|t| format!("<aside><strong>Copie agora; o token não será mostrado novamente.</strong><code>{}</code><pre>CFP_SERVER=wss://a.rotava.com CFP_TOKEN=\"{}\" ./cfp-client</pre></aside>", escape(t), escape(t))).unwrap_or_default();
    format!(
        r#"<!doctype html><html lang=pt-br><meta charset=utf-8><meta name=viewport content="width=device-width"><title>cf-p</title><style>body{{font:16px system-ui;max-width:900px;margin:40px auto;padding:0 16px;background:#10131a;color:#e8edf5}}h1{{color:#77d5ff}}section,aside{{background:#1b2130;padding:20px;margin:18px 0;border-radius:12px}}code,pre{{background:#090b10;padding:5px;border-radius:5px;overflow:auto}}aside code{{display:block;margin:14px 0}}form{{display:flex;gap:8px;flex-wrap:wrap;margin:12px 0}}input{{flex:1;min-width:220px;padding:10px}}button{{padding:10px;background:#168aad;color:white;border:0;border-radius:6px}}.danger{{background:#9b2c2c}}.route{{display:flex;align-items:center;gap:8px;flex-wrap:wrap;border-top:1px solid #343b4b}}</style><h1>cf-p</h1><p>Configuração ativa. Alterações são salvas no YAML e os clientes reconectam automaticamente.</p>{notice}<form method=post action=/clients><button>Novo cliente + token</button></form>{cards}</html>"#
    )
}

fn save_config(path: &Path, config: &Config) -> Result<()> {
    let temporary = path.with_extension("yaml.tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(serde_yaml::to_string(config)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

async fn http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    extra: &[&str],
) -> Result<()> {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!("{}\r\n", extra.join("\r\n"))
    };
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Frame-Options: DENY\r\n{extra}Connection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
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
    if let Some(admin) = &config.admin {
        if admin.username.trim().is_empty() || admin.password.len() < 12 {
            anyhow::bail!("admin exige username e password com pelo menos 12 caracteres");
        }
    }
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
            admin: None,
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

    #[test]
    fn decodes_admin_form_and_checks_origin() {
        let form = parse_form(b"listen=b.rotava.com&target=127.0.0.1%3A3000");
        assert_eq!(field(&form, "target").unwrap(), "127.0.0.1:3000");
        assert!(same_origin_or_absent(
            "Host: a.rotava.com\r\nOrigin: https://a.rotava.com\r\n"
        ));
        assert!(!same_origin_or_absent(
            "Host: a.rotava.com\r\nOrigin: https://evil.example\r\n"
        ));
    }
}
