use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use cfp_protocol::*;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex, mpsc},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "CFP_SERVER")]
    server: String,
    #[arg(long, env = "CFP_TOKEN")]
    token: String,
}

type Streams = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    let mut delay = 1;
    loop {
        let started = Instant::now();
        if let Err(error) = run(&args).await {
            // `{error:#}` inclui toda a cadeia de causas do anyhow. Sem isso,
            // erros de DNS, TCP, TLS e handshake apareciam apenas como
            // "falha ao conectar WSS", dificultando diagnóstico no Windows.
            warn!(error = %format!("{error:#}"), "tunel desconectado");
        }
        // Uma sessão que durou reinicia o backoff: sem isso, um túnel de horas
        // derrubado por uma reconfiguração esperaria os 30s do teto para voltar.
        if started.elapsed() >= Duration::from_secs(60) {
            delay = 1;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

async fn run(args: &Args) -> Result<()> {
    let (ws, _) = connect_async(&args.server)
        .await
        .context("falha ao conectar WSS")?;
    let (mut writer, mut reader) = ws.split();
    writer
        .send(Message::Binary(
            Frame::new(AUTH, 0, args.token.as_bytes()).encode().into(),
        ))
        .await?;
    let response = reader
        .next()
        .await
        .context("servidor fechou durante auth")??;
    let frame = Frame::decode(&response.into_data())?;
    if frame.kind == ERROR {
        anyhow::bail!(
            "servidor recusou a autenticacao: {}",
            String::from_utf8_lossy(&frame.payload)
        );
    }
    if frame.kind != AUTH_OK {
        anyhow::bail!("resposta de autenticacao inesperada: {}", frame.kind);
    }
    info!("tunel autenticado");

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let write_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            writer.send(Message::Binary(frame.encode().into())).await?;
        }
        anyhow::Ok(())
    });
    let streams: Streams = Default::default();

    while let Some(message) = reader.next().await {
        let message = message?;
        if !message.is_binary() {
            continue;
        }
        let frame = Frame::decode(&message.into_data())?;
        match frame.kind {
            OPEN => open_stream(frame, out_tx.clone(), streams.clone()).await,
            DATA => {
                if let Some(tx) = streams.lock().await.get(&frame.stream_id).cloned() {
                    let _ = tx.send(frame.payload).await;
                }
            }
            CLOSE => {
                streams.lock().await.remove(&frame.stream_id);
            }
            _ => {}
        }
    }
    write_task.abort();
    anyhow::bail!("servidor encerrou a sessao")
}

async fn open_stream(frame: Frame, out: mpsc::Sender<Frame>, streams: Streams) {
    let target = match String::from_utf8(frame.payload) {
        Ok(target) => target,
        Err(_) => return,
    };
    let id = frame.stream_id;
    match TcpStream::connect(&target).await {
        Ok(stream) => {
            let (mut read, mut write) = stream.into_split();
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
            streams.lock().await.insert(id, tx);
            let _ = out.send(Frame::new(OPEN_OK, id, [])).await;
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
                let _ = write.shutdown().await;
            });
        }
        Err(error) => {
            let _ = out
                .send(Frame::new(OPEN_ERROR, id, error.to_string()))
                .await;
        }
    }
}

