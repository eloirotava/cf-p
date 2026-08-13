use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Write,
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, mpsc, watch},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "server.yaml")]
    config: String,
}
#[derive(Clone, Deserialize, Serialize)]
struct Route {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    listen: String,
    target: String,
}
#[derive(Clone, Deserialize, Serialize)]
struct Client {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    /// Token em texto puro, guardado para o painel poder reexibi-lo. A
    /// autenticacao continua comparando `token_sha256`, entao clientes criados
    /// antes deste campo seguem funcionando sem ele -- apenas nao ha o que
    /// mostrar. Quem obtiver este arquivo obtem acesso direto aos tuneis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    token_sha256: String,
    routes: Vec<Route>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Config {
    listen: String,
    #[serde(default = "default_http_listen")]
    http_listen: String,
    admin: Option<AdminConfig>,
    clients: Vec<Client>,
}
#[derive(Clone, Deserialize, Serialize)]
struct AdminConfig {
    #[serde(default = "default_admin_listen")]
    listen: String,
    #[serde(default = "default_public_url")]
    public_url: String,
    username: String,
    password: String,
}
type Streams = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;
type DomainRoutes = Arc<RwLock<HashMap<String, DomainRoute>>>;
/// Ultimo estado conhecido de cada cliente, indexado por `token_sha256`.
type Presencas = Arc<RwLock<HashMap<String, Presenca>>>;
/// Geracao de configuracao por cliente. Uma sessao so reconecta quando a
/// geracao do proprio token muda, entao mexer num cliente nao derruba os outros.
type Geracoes = Arc<HashMap<String, u64>>;

struct Presenca {
    conectado: bool,
    desde: std::time::SystemTime,
    peer: String,
    session_id: u64,
}

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
fn default_public_url() -> String {
    "wss://a.rotava.com".into()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let config_path = PathBuf::from(Args::parse().config);
    let cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
    validate_config(&cfg)?;
    let listener = TcpListener::bind(&cfg.listen).await?;
    info!(listen = %cfg.listen, "servidor iniciado");
    let clients = Arc::new(RwLock::new(cfg.clients.clone()));
    let presencas: Presencas = Default::default();
    let (reload_tx, reload_rx) = watch::channel(Geracoes::default());
    if let Some(admin) = cfg.admin.clone() {
        let listener = TcpListener::bind(&admin.listen).await?;
        info!(listen = %admin.listen, "painel administrativo iniciado");
        tokio::spawn(serve_admin(
            listener,
            admin,
            config_path,
            clients.clone(),
            presencas.clone(),
            reload_tx.clone(),
        ));
    }
    let domains: DomainRoutes = Default::default();
    let http_listener = TcpListener::bind(&cfg.http_listen).await?;
    info!(listen = %cfg.http_listen, "roteador HTTP por dominio iniciado");
    tokio::spawn(serve_http(http_listener, domains.clone()));
    loop {
        let (tcp, peer) = listener.accept().await?;
        let clients = clients.clone();
        let reload = session_reload(&reload_rx);
        let domains = domains.clone();
        let presencas = presencas.clone();
        tokio::spawn(async move {
            let result = async {
                let ws = accept_async(tcp).await?;
                serve(ws, peer.to_string(), clients, domains, presencas, reload).await
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "sessao encerrada");
            }
        });
    }
}

/// Deriva o receptor de recarga de uma nova sessao.
///
/// O receptor mestre em `main` nunca observa as versoes do watch, e um clone
/// herda a versao ja vista pela origem. Sem marcar o clone como atualizado,
/// toda sessao aberta depois do primeiro POST no painel nasceria atrasada:
/// `changed()` retornaria imediatamente e derrubaria o cliente logo apos o
/// AUTH_OK, num laco infinito de reconexao. Marcado aqui, cada sessao so
/// reage as alteracoes de configuracao posteriores a ela.
fn session_reload(master: &watch::Receiver<Geracoes>) -> watch::Receiver<Geracoes> {
    let mut reload = master.clone();
    reload.mark_unchanged();
    reload
}

async fn serve<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    peer: String,
    clients: Arc<RwLock<Vec<Client>>>,
    domains: DomainRoutes,
    presencas: Presencas,
    mut reload: watch::Receiver<Geracoes>,
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
    let minha_geracao = reload.borrow().get(&digest).copied().unwrap_or(0);
    presencas.write().await.insert(
        digest.clone(),
        Presenca {
            conectado: true,
            desde: std::time::SystemTime::now(),
            peer,
            session_id,
        },
    );
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
                if changed.is_err() {
                    break;
                }
                // So reconecta se a alteracao foi neste cliente: mexer na rota
                // de um nao pode derrubar a sessao dos outros.
                if reload.borrow().get(&digest).copied().unwrap_or(0) != minha_geracao {
                    info!(session_id, "configuracao deste cliente alterada; reconectando");
                    break;
                }
                continue;
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
    // Uma sessao antiga que termina depois de o cliente ja ter reconectado nao
    // pode marcar a nova como desconectada.
    if let Some(presenca) = presencas.write().await.get_mut(&digest) {
        if presenca.session_id == session_id {
            presenca.conectado = false;
            presenca.desde = std::time::SystemTime::now();
        }
    }
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
    presencas: Presencas,
    reload: watch::Sender<Geracoes>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let (admin, path, clients, presencas, reload) = (
                    admin.clone(),
                    config_path.clone(),
                    clients.clone(),
                    presencas.clone(),
                    reload.clone(),
                );
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_admin(stream, admin, path, clients, presencas, reload).await
                    {
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
    presencas: Presencas,
    reload: watch::Sender<Geracoes>,
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
        // Erro aqui e culpa do preenchimento, nao do servidor: renderiza o
        // painel com a mensagem em vez de derrubar a conexao sem resposta.
        let (alterado, afetados) = match aplicar(path, &form, &config_path) {
            Ok(Some(resultado)) => resultado,
            Ok(None) => {
                return http_response(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    "Nao encontrado",
                    &[],
                )
                .await;
            }
            Err(error) => {
                let cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
                return http_response(
                    &mut stream,
                    "422 Unprocessable Content",
                    "text/html; charset=utf-8",
                    &render_admin(&cfg, Some(&format!("{error:#}")), &*presencas.read().await),
                    &[],
                )
                .await;
            }
        };
        save_config(&config_path, &alterado)?;
        *clients.write().await = alterado.clients.clone();
        reload.send_modify(|geracoes| {
            let mut novas = HashMap::clone(geracoes);
            for token in &afetados {
                *novas.entry(token.clone()).or_default() += 1;
            }
            *geracoes = Arc::new(novas);
        });
        return http_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            &render_admin(&alterado, None, &*presencas.read().await),
            &[],
        )
        .await;
    }
    let cfg: Config = serde_yaml::from_reader(File::open(&config_path)?)?;
    http_response(
        &mut stream,
        "200 OK",
        "text/html; charset=utf-8",
        &render_admin(&cfg, None, &*presencas.read().await),
        &[],
    )
    .await
}

/// Aplica a alteracao pedida e devolve a configuracao validada.
///
/// `Ok(None)` significa caminho desconhecido. Qualquer `Err` e problema do
/// formulario -- campo faltando, indice inexistente, listen duplicado -- e o
/// chamador o transforma em mensagem na pagina.
fn aplicar(
    path: &str,
    form: &HashMap<String, String>,
    config_path: &Path,
) -> Result<Option<(Config, Vec<String>)>> {
    let mut cfg: Config = serde_yaml::from_reader(File::open(config_path)?)?;
    // `token_sha256` dos clientes cuja sessao precisa cair. Renomear nao entra:
    // nome e cosmetico e nao justifica derrubar um tunel.
    let mut afetados: Vec<String> = Vec::new();
    let mut afetar = |cliente: &Client| afetados.push(cliente.token_sha256.to_ascii_lowercase());
    match path {
            "/clients" => {
                let mut bytes = [0_u8; 32];
                rand::rng().fill_bytes(&mut bytes);
                let token = hex::encode(bytes);
                cfg.clients.push(Client {
                    name: opcional(&form, "name").into(),
                    token_sha256: hex::encode(Sha256::digest(token.as_bytes())),
                    token: Some(token),
                    routes: vec![],
                });
            }
            "/rename-client" => {
                let index: usize = field(&form, "client")?.parse()?;
                let client = cfg.clients.get_mut(index).context("cliente inexistente")?;
                client.name = opcional(&form, "name").into();
            }
            "/rotate-token" => {
                let index: usize = field(&form, "client")?.parse()?;
                let client = cfg.clients.get_mut(index).context("cliente inexistente")?;
                afetar(client);
                let mut bytes = [0_u8; 32];
                rand::rng().fill_bytes(&mut bytes);
                let token = hex::encode(bytes);
                client.token_sha256 = hex::encode(Sha256::digest(token.as_bytes()));
                client.token = Some(token);
            }
            "/routes" => {
                let index: usize = field(&form, "client")?.parse()?;
                let client = cfg.clients.get_mut(index).context("cliente inexistente")?;
                afetar(client);
                client.routes.push(Route {
                    name: opcional(&form, "name").into(),
                    listen: field(&form, "listen")?.into(),
                    target: field(&form, "target")?.into(),
                });
            }
            "/edit-route" => {
                let client: usize = field(&form, "client")?.parse()?;
                let index: usize = field(&form, "route")?.parse()?;
                let (listen, target) = (field(&form, "listen")?, field(&form, "target")?);
                let alvo = cfg.clients.get(client).context("cliente inexistente")?;
                afetar(alvo);
                let route = cfg
                    .clients
                    .get_mut(client)
                    .context("cliente inexistente")?
                    .routes
                    .get_mut(index)
                    .context("rota inexistente")?;
                route.name = opcional(&form, "name").into();
                route.listen = listen.into();
                route.target = target.into();
            }
            "/delete-route" => {
                let client: usize = field(&form, "client")?.parse()?;
                let route: usize = field(&form, "route")?.parse()?;
                let alvo = cfg.clients.get(client).context("cliente inexistente")?;
                alvo.routes.get(route).context("rota inexistente")?;
                afetar(alvo);
                cfg.clients[client].routes.remove(route);
            }
            "/delete-client" => {
                let index: usize = field(&form, "client")?.parse()?;
                // O cliente removido precisa ser desconectado: sem isso a sessao
                // aberta continuaria servindo rotas que nao existem mais.
                afetar(cfg.clients.get(index).context("cliente inexistente")?);
                cfg.clients.remove(index);
            }
            _ => return Ok(None),
        }
        validate_config(&cfg)?;
        drop(afetar);
        Ok(Some((cfg, afetados)))
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
/// Campo cuja ausencia nao e erro, como os nomes livres do painel.
fn opcional<'a>(form: &'a HashMap<String, String>, name: &str) -> &'a str {
    form.get(name).map(|v| v.trim()).unwrap_or_default()
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Ha quanto tempo, em texto curto. O painel precisa responder "desde quando",
/// nao a hora exata, e isso dispensa uma dependencia de formatacao de data.
fn ha(desde: std::time::SystemTime) -> String {
    let s = std::time::SystemTime::now()
        .duration_since(desde)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}min", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn render_admin(
    cfg: &Config,
    erro: Option<&str>,
    presencas: &HashMap<String, Presenca>,
) -> String {
    let server = cfg
        .admin
        .as_ref()
        .map(|admin| admin.public_url.as_str())
        .unwrap_or("wss://a.rotava.com");
    let mut cards = String::new();
    for (ci, client) in cfg.clients.iter().enumerate() {
        let titulo = if client.name.is_empty() {
            format!("Cliente {}", ci + 1)
        } else {
            escape(&client.name)
        };
        let estado = match presencas.get(&client.token_sha256.to_ascii_lowercase()) {
            Some(p) if p.conectado => format!(
                "<p class=estado><b class=on>● conectado</b> há {} — de {}</p>",
                ha(p.desde),
                escape(&p.peer)
            ),
            Some(p) => format!(
                "<p class=estado><b class=off>● desconectado</b> há {}</p>",
                ha(p.desde)
            ),
            None => "<p class=estado><b class=off>● nunca conectou</b> desde que o servidor subiu</p>"
                .to_string(),
        };
        cards.push_str(&format!(
            "<section><h2>{titulo}</h2>{estado}\
             <form method=post action=/rename-client><input type=hidden name=client value={ci}>\
             <input name=name value=\"{}\" placeholder='nome do cliente, ex.: bananapi'>\
             <button>Renomear</button></form>",
            escape(&client.name)
        ));

        cards.push_str(&match &client.token {
            Some(token) => format!(
                "<div class=token><code>{}</code>\
                 <details><summary>Como executar</summary>\
                 <h3>Linux / macOS</h3><pre>CFP_SERVER=\"{}\" CFP_TOKEN=\"{}\" ./cfp-client</pre>\
                 <h3>Windows PowerShell</h3><pre>$env:CFP_SERVER=\"{}\"; $env:CFP_TOKEN=\"{}\"; .\\cfp-client.exe</pre>\
                 </details></div>",
                escape(token),
                escape(server),
                escape(token),
                escape(server),
                escape(token)
            ),
            None => format!(
                "<div class=token><em>Token criado antes do painel guardar o valor; \
                 só o hash <code>{}…</code> existe. Gere um novo para poder vê-lo.</em></div>",
                escape(&client.token_sha256[..client.token_sha256.len().min(12)])
            ),
        });

        for (ri, route) in client.routes.iter().enumerate() {
            cards.push_str(&format!(
                "<div class=route><form method=post action=/edit-route>\
                 <input type=hidden name=client value={ci}><input type=hidden name=route value={ri}>\
                 <input name=name value=\"{}\" placeholder='nome do túnel'>\
                 <input name=listen value=\"{}\" required><input name=target value=\"{}\" required>\
                 <button>Salvar</button></form>\
                 <form method=post action=/delete-route onsubmit=\"return confirm('Excluir esta rota?')\">\
                 <input type=hidden name=client value={ci}>\
                 <input type=hidden name=route value={ri}><button class=danger>Excluir</button></form></div>",
                escape(&route.name),
                escape(&route.listen),
                escape(&route.target)
            ));
        }

        cards.push_str(&format!(
            "<form method=post action=/routes><input type=hidden name=client value={ci}>\
             <input name=name placeholder='nome do túnel'>\
             <input name=listen required placeholder='b.rotava.com ou 0.0.0.0:33890'>\
             <input name=target required placeholder='127.0.0.1:3000'>\
             <button>Adicionar rota</button></form>\
             <div class=perigo><form method=post action=/rotate-token onsubmit=\"return confirm('Gerar novo token? O token atual para de funcionar e o cliente cai até ser reconfigurado.')\">\
             <input type=hidden name=client value={ci}>\
             <button class=danger>Gerar novo token</button></form>\
             <form method=post action=/delete-client onsubmit=\"return confirm('Excluir o cliente, suas rotas e seu token? Não há como desfazer.')\">\
             <input type=hidden name=client value={ci}>\
             <button class=danger>Excluir cliente</button></form></div></section>"
        ));
    }
    let aviso = erro
        .map(|e| format!("<p class=erro>{}</p>", escape(e)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang=pt-br><meta charset=utf-8><meta name=viewport content="width=device-width"><title>cf-p</title><style>body{{font:16px system-ui;max-width:900px;margin:40px auto;padding:0 16px;background:#10131a;color:#e8edf5}}h1{{color:#77d5ff}}h2{{margin:0 0 4px}}h3{{margin:14px 0 4px;font-size:14px;color:#9fb0c8}}section{{background:#1b2130;padding:20px;margin:18px 0;border-radius:12px}}code,pre{{background:#090b10;padding:5px;border-radius:5px;overflow:auto}}.token code{{display:block;margin:10px 0;word-break:break-all}}summary{{cursor:pointer;color:#77d5ff;font-size:14px}}form{{display:flex;gap:8px;flex-wrap:wrap;margin:12px 0}}input{{flex:1;min-width:180px;padding:10px;background:#0d1017;color:#e8edf5;border:1px solid #343b4b;border-radius:6px}}button{{padding:10px;background:#168aad;color:white;border:0;border-radius:6px;cursor:pointer}}.danger{{background:#9b2c2c}}.route{{display:flex;align-items:center;gap:8px;flex-wrap:wrap;border-top:1px solid #343b4b;padding-top:6px}}.perigo{{display:flex;gap:8px;border-top:1px solid #343b4b;padding-top:12px;margin-top:12px}}.perigo form{{margin:0}}.estado{{margin:0 0 10px;font-size:14px}}.on{{color:#4ade80}}.off{{color:#f87171}}.erro{{background:#4a1d1d;border:1px solid #9b2c2c;padding:12px;border-radius:8px}}</style><h1>cf-p</h1>{aviso}<p>Configuração ativa. Alterações são salvas no YAML e os clientes reconectam automaticamente.</p><form method=post action=/clients><input name=name placeholder='nome do cliente, ex.: bananapi'><button>Novo cliente + token</button></form>{cards}</html>"#
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
        if !admin.public_url.starts_with("wss://") {
            anyhow::bail!("admin.public_url deve comecar com wss://");
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
            name: String::new(),
            token: None,
            token_sha256: token.into(),
            routes: vec![Route {
                name: String::new(),
                listen: listen.into(),
                target: "127.0.0.1:1".into(),
            }],
        };
        let token = "a".repeat(64);
        let mut config = Config {
            listen: "127.0.0.1:444".into(),
            http_listen: "127.0.0.1:445".into(),
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

    #[test]
    fn aplicar_renomeia_e_recusa_listen_duplicado() {
        let path = std::env::temp_dir().join(format!("cfp-teste-{}.yaml", std::process::id()));
        let base = Config {
            listen: "127.0.0.1:444".into(),
            http_listen: "127.0.0.1:445".into(),
            admin: None,
            clients: vec![Client {
                name: String::new(),
                token: None,
                token_sha256: "a".repeat(64),
                routes: vec![Route {
                    name: String::new(),
                    listen: "b.rotava.com".into(),
                    target: "127.0.0.1:1".into(),
                }],
            }],
        };
        save_config(&path, &base).unwrap();
        let form = |pares: &[(&str, &str)]| -> HashMap<String, String> {
            pares
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };

        let (renomeado, afetados) = aplicar(
            "/rename-client",
            &form(&[("client", "0"), ("name", "bananapi")]),
            &path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(renomeado.clients[0].name, "bananapi");
        // Nome e cosmetico: renomear nao pode derrubar o tunel.
        assert!(afetados.is_empty());

        let (_, afetados) = aplicar(
            "/routes",
            &form(&[
                ("client", "0"),
                ("listen", "c.rotava.com"),
                ("target", "127.0.0.1:3"),
            ]),
            &path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(afetados, vec!["a".repeat(64)]);

        // O mesmo hostname em outra rota precisa ser recusado, e como erro do
        // formulario -- nao como falha que derruba a conexao sem resposta.
        let duplicado = aplicar(
            "/routes",
            &form(&[
                ("client", "0"),
                ("listen", "B.Rotava.com"),
                ("target", "127.0.0.1:2"),
            ]),
            &path,
        );
        assert!(duplicado.is_err());

        assert!(aplicar("/inexistente", &form(&[]), &path).unwrap().is_none());
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn session_ignores_reloads_anteriores_a_ela() {
        let bump = |tx: &watch::Sender<Geracoes>, token: &str| {
            tx.send_modify(|geracoes| {
                let mut novas = HashMap::clone(geracoes);
                *novas.entry(token.to_string()).or_default() += 1;
                *geracoes = Arc::new(novas);
            })
        };
        let (tx, master) = watch::channel(Geracoes::default());
        bump(&tx, "meu-token");

        let mut reload = session_reload(&master);
        let immediate =
            tokio::time::timeout(std::time::Duration::from_millis(50), reload.changed()).await;
        assert!(
            immediate.is_err(),
            "sessao caiu por alteracao anterior a ela"
        );

        bump(&tx, "meu-token");
        assert!(
            reload.changed().await.is_ok(),
            "sessao ignorou alteracao posterior"
        );
    }

    /// A geracao e por cliente: mexer num nao pode derrubar a sessao do outro.
    #[tokio::test]
    async fn geracao_isola_clientes() {
        let (tx, master) = watch::channel(Geracoes::default());
        let mut reload = session_reload(&master);
        let minha = reload.borrow().get("meu-token").copied().unwrap_or(0);

        tx.send_modify(|geracoes| {
            let mut novas = HashMap::clone(geracoes);
            *novas.entry("outro-token".to_string()).or_default() += 1;
            *geracoes = Arc::new(novas);
        });

        assert!(reload.changed().await.is_ok(), "o watch nao notificou");
        assert_eq!(
            reload.borrow().get("meu-token").copied().unwrap_or(0),
            minha,
            "alteracao em outro cliente mudou a geracao desta sessao"
        );
    }

}
