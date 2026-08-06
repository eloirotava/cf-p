use std::{
    collections::HashMap,
    fs::File,
    io::BufReader,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
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
    sync::{Mutex, mpsc},
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
    cert: String,
    key: String,
    clients: Vec<Client>,
}
type Streams = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg: Config = serde_yaml::from_reader(File::open(Args::parse().config)?)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config(&cfg.cert, &cfg.key)?));
    let listener = TcpListener::bind(&cfg.listen).await?;
    info!(listen = %cfg.listen, "servidor iniciado");
    let clients = Arc::new(cfg.clients);
    loop {
        let (tcp, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let clients = clients.clone();
        tokio::spawn(async move {
            let result = async {
                let tls = acceptor.accept(tcp).await?;
                let ws = accept_async(tls).await?;
                serve(ws, clients).await
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "sessao encerrada");
            }
        });
    }
}

async fn serve<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    clients: Arc<Vec<Client>>,
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
        .context("token invalido")?
        .clone();
    writer
        .send(Message::Binary(Frame::new(AUTH_OK, 0, []).encode().into()))
        .await?;

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let streams: Streams = Default::default();
    let mut route_tasks = Vec::new();
    for route in client.routes {
        let tx = out_tx.clone();
        let streams = streams.clone();
        route_tasks.push(tokio::spawn(async move {
            if let Err(e) = route_listener(route, tx, streams).await {
                error!(%e, "rota parou");
            }
        }));
    }
    let write_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            writer.send(Message::Binary(frame.encode().into())).await?;
        }
        anyhow::Ok(())
    });
    while let Some(message) = reader.next().await {
        let message = message?;
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
) {
    let (mut read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    streams.lock().await.insert(id, tx);
    if out.send(Frame::new(OPEN, id, target)).await.is_err() {
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

fn tls_config(cert: &str, key: &str) -> Result<ServerConfig> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(cert)?))
        .collect::<std::io::Result<Vec<_>>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(key)?))?
        .context("chave privada ausente")?;
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}
