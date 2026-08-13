use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cfp_protocol::*;
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

struct Args {
    server: String,
    token: String,
}

impl Args {
    /// Le a configuracao do ambiente. `clap` fazia o mesmo por derive, mas
    /// custava ~185 KB no binario para duas variaveis; o painel ja documenta a
    /// execucao por `CFP_SERVER`/`CFP_TOKEN`.
    fn from_env() -> Result<Self> {
        Ok(Self {
            server: var("CFP_SERVER")?,
            token: var("CFP_TOKEN")?,
        })
    }
}

fn var(nome: &str) -> Result<String> {
    std::env::var(nome).with_context(|| format!("defina a variavel de ambiente {nome}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let args = Args::from_env()?;
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

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(FILA_SESSAO);
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
                let id = frame.stream_id;
                // Nada aqui espera: um stream lento nao pode mais parar a
                // leitura da sessao inteira. Estourar a janela e violacao de
                // protocolo, e derrubar a sessao e melhor que acumular.
                if let Some(state) = streams.lock().await.get(&id) {
                    state
                        .receber(frame.payload)
                        .with_context(|| format!("stream {id}"))?;
                }
            }
            WINDOW_UPDATE => {
                if let Some(valor) = frame.valor_credito() {
                    if let Some(state) = streams.lock().await.get(&frame.stream_id) {
                        state.creditar(valor);
                    }
                }
            }
            CLOSE => {
                if let Some(state) = streams.lock().await.remove(&frame.stream_id) {
                    state.encerrar();
                }
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
            // OPEN_OK antes da ponte: `out` preserva ordem, entao o servidor
            // nunca ve DATA de um stream que ele ainda considera abrindo.
            let _ = out.send(Frame::new(OPEN_OK, id, [])).await;
            bridge(stream, id, out, streams, Vec::new()).await;
        }
        Err(error) => {
            let _ = out
                .send(Frame::new(OPEN_ERROR, id, error.to_string()))
                .await;
        }
    }
}

