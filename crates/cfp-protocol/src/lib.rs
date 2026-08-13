use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex, Semaphore, mpsc},
};

pub const AUTH: u8 = 1;
pub const AUTH_OK: u8 = 2;
pub const OPEN: u8 = 3;
pub const OPEN_OK: u8 = 4;
pub const OPEN_ERROR: u8 = 5;
pub const DATA: u8 = 6;
pub const CLOSE: u8 = 7;
pub const ERROR: u8 = 8;
pub const WINDOW_UPDATE: u8 = 9;

/// Versao da wire. Subiu de 1 para 2 junto com o controle de fluxo: um binario
/// antigo autenticaria normalmente e so travaria quando o primeiro credito
/// acabasse, o que e muito pior de diagnosticar do que recusar o frame aqui.
pub const VERSAO: u8 = 2;

/// Bytes que cada lado pode ter em voo num stream antes de esperar credito.
///
/// E o teto de vazao de um stream sozinho: `JANELA / RTT`. Com 1 MiB e os
/// 164 ms medidos ate a VPS da ~51 Mbit/s, acima do link. Diminuir limita
/// transferencia grande; aumentar nao acelera nada, porque acima do BDP o
/// excedente vira fila, e fila vira latencia.
pub const JANELA: u32 = 1024 * 1024;

/// Quanto o receptor consome antes de devolver credito. Meia janela mantem o
/// emissor com folga e gera um WINDOW_UPDATE a cada ~512 KB, em vez de um por
/// frame de 16 KB.
pub const LIMIAR_UPDATE: u32 = JANELA / 2;

/// Maior leitura por vez do socket local, e portanto maior payload de um DATA.
pub const CHUNK: usize = 16 * 1024;

/// Profundidade da fila compartilhada por todos os streams da sessao.
///
/// Pequena de proposito. O BDP do caminho tem que morar num lugar so -- a
/// janela por stream -- e nao empilhado tambem aqui: era esta fila, com 256
/// frames de 16 KB, que segurava ~4 MB e punha qualquer stream interativo
/// atras de segundos de dados alheios.
pub const FILA_SESSAO: usize = 16;

#[derive(Debug)]
pub struct Frame {
    pub kind: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, stream_id: u32, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            stream_id,
            payload: payload.into(),
        }
    }

    pub fn credito(stream_id: u32, bytes: u32) -> Self {
        Self::new(WINDOW_UPDATE, stream_id, bytes.to_be_bytes())
    }

    /// Le o valor de um WINDOW_UPDATE. `None` se o payload nao tiver os 4 bytes.
    pub fn valor_credito(&self) -> Option<u32> {
        let bytes: [u8; 4] = self.payload.get(..4)?.try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    }

    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.payload.len());
        out.push(VERSAO);
        out.push(self.kind);
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 6 {
            bail!("frame curto");
        }
        if data[0] != VERSAO {
            bail!(
                "versao de protocolo {} incompativel: este binario fala {VERSAO}",
                data[0]
            );
        }
        Ok(Self {
            kind: data[1],
            stream_id: u32::from_be_bytes(data[2..6].try_into()?),
            payload: data[6..].to_vec(),
        })
    }
}

/// Estado de um stream. Identico nos dois lados: o cliente e o servidor rodam
/// exatamente este codigo, porque um controle de fluxo em que as pontas
/// discordam da contabilidade trava sem dizer por que.
pub struct StreamState {
    /// Fila ate o socket local. Deliberadamente ilimitada: quem limita a
    /// memoria e o credito. Uma segunda fronteira contada em mensagens
    /// conflitaria com uma janela contada em bytes -- 64 frames de 1 byte
    /// encheriam uma fila de 64 posicoes com a janela quase toda em aberto.
    tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes recebidos e ainda nao escritos no socket local.
    pendente: Arc<AtomicU32>,
    /// Bytes que ainda podemos enviar. Fechar o semaforo acorda quem espera,
    /// que e o que impede um stream encerrado de deixar a task leitora
    /// dormindo para sempre.
    credito: Arc<Semaphore>,
}

impl StreamState {
    /// Entrega um DATA recebido. Nunca bloqueia -- e esse o ponto: enquanto o
    /// laco de demux esperava numa fila limitada, um unico stream lento
    /// congelava todos os outros da sessao, inclusive os CLOSE deles.
    pub fn receber(&self, dados: Vec<u8>) -> Result<()> {
        let n = dados.len() as u32;
        if self.pendente.fetch_add(n, Ordering::AcqRel).saturating_add(n) > JANELA {
            bail!("par enviou alem da janela de {JANELA} bytes");
        }
        let _ = self.tx.send(dados);
        Ok(())
    }

    pub fn creditar(&self, bytes: u32) {
        self.credito.add_permits(bytes as usize);
    }

    /// Acorda a task leitora para ela ver que o stream acabou.
    pub fn encerrar(&self) {
        self.credito.close();
    }
}

pub type Streams = Arc<Mutex<HashMap<u32, StreamState>>>;

/// Liga um socket local a um stream do tunel, com controle de fluxo nas duas
/// direcoes. O chamador ja deve ter enviado o OPEN ou o OPEN_OK: a ordem se
/// mantem porque `out` preserva a ordem de envio.
///
/// `inicial` sao bytes ja lidos do socket antes da ponte existir -- o servidor
/// consome os headers HTTP para descobrir o Host e precisa repassa-los.
pub async fn bridge(
    stream: TcpStream,
    id: u32,
    out: mpsc::Sender<Frame>,
    streams: Streams,
    inicial: Vec<u8>,
) {
    // Sem isso o Nagle segura pedacos pequenos esperando encher um segmento, e
    // do outro lado o delayed ACK espera dados: as duas esperas se somam em
    // paradas de ate 40 ms por troca, que e o que faz SSH e RDP arrastarem.
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pendente = Arc::new(AtomicU32::new(0));
    let credito = Arc::new(Semaphore::new(JANELA as usize));
    streams.lock().await.insert(
        id,
        StreamState {
            tx,
            pendente: pendente.clone(),
            credito: credito.clone(),
        },
    );

    // Local -> tunel.
    let out_read = out.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0; CHUNK];
        let mut inicial = inicial;
        loop {
            let dados = if inicial.is_empty() {
                match read.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer[..n].to_vec(),
                }
            } else {
                std::mem::take(&mut inicial)
            };
            // Gastar credito depois de ler mantem o excedente local limitado a
            // um CHUNK: a proxima leitura so acontece quando o par devolver
            // janela, e ai a contrapressao chega ao socket de origem -- que e
            // onde ela pertence, e nao no laco compartilhado da sessao.
            match credito.acquire_many(dados.len() as u32).await {
                Ok(permit) => permit.forget(),
                Err(_) => break,
            }
            if out_read.send(Frame::new(DATA, id, dados)).await.is_err() {
                break;
            }
        }
        let _ = out_read.send(Frame::new(CLOSE, id, [])).await;
    });

    // Tunel -> local.
    tokio::spawn(async move {
        let mut acumulado = 0_u32;
        while let Some(dados) = rx.recv().await {
            let n = dados.len() as u32;
            if write.write_all(&dados).await.is_err() {
                break;
            }
            // Devolver credito so depois do write_all e o que faz a janela
            // medir o que o socket local deu conta de consumir. Devolver ao
            // receber mediria o tamanho da fila, e a fila voltaria a crescer.
            pendente.fetch_sub(n, Ordering::AcqRel);
            acumulado += n;
            if acumulado >= LIMIAR_UPDATE {
                if out.send(Frame::credito(id, acumulado)).await.is_err() {
                    break;
                }
                acumulado = 0;
            }
        }
        let _ = write.shutdown().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let decoded = Frame::decode(&Frame::new(DATA, 42, b"abc").encode()).unwrap();
        assert_eq!(decoded.kind, DATA);
        assert_eq!(decoded.stream_id, 42);
        assert_eq!(decoded.payload, b"abc");
    }

    #[test]
    fn rejects_short_or_unknown_version() {
        assert!(Frame::decode(&[VERSAO, DATA]).is_err());
        assert!(Frame::decode(&[1, DATA, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn credito_round_trip() {
        let frame = Frame::decode(&Frame::credito(7, 512 * 1024).encode()).unwrap();
        assert_eq!(frame.kind, WINDOW_UPDATE);
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.valor_credito(), Some(512 * 1024));
    }

    #[test]
    fn window_update_curto_nao_entra_em_panico() {
        assert_eq!(Frame::new(WINDOW_UPDATE, 1, b"ab").valor_credito(), None);
    }

    /// A janela tem que caber no semaforo e o limiar tem que ser menor que ela,
    /// senao o credito nunca volta.
    #[test]
    fn janela_coerente() {
        assert!(LIMIAR_UPDATE < JANELA);
        assert!((JANELA as usize) < Semaphore::MAX_PERMITS);
        assert!(CHUNK < JANELA as usize);
    }
}
