#![forbid(unsafe_code)]

use std::io;

use ferrite_core::{MAX_EXT_TO_HOST_BYTES, MAX_HOST_TO_EXT_BYTES};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::codec::{read_frame_async, read_frame_sync, write_frame_async, write_frame_sync};

const CHANNEL_CAPACITY: usize = 256;

/// Dedicated OS-thread stdio Native Messaging bridge for production Chrome Native Host runs.
///
/// Blocking `stdin` and `stdout` pipe reads/writes are isolated onto named OS threads so Windows
/// anonymous pipe I/O never stalls Tokio worker threads, and `stdout` remains strictly reserved for
/// 4-byte LE framed Native Messaging packets.
pub struct StdioBridge {
    pub outbound_tx: mpsc::Sender<Vec<u8>>,
    pub inbound_rx: mpsc::Receiver<Vec<u8>>,
}

impl StdioBridge {
    pub fn spawn() -> io::Result<Self> {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

        std::thread::Builder::new()
            .name("ferrite-nm-stdin".to_string())
            .spawn(move || {
                let stdin = io::stdin();
                let mut locked = stdin.lock();
                loop {
                    match read_frame_sync(&mut locked, MAX_EXT_TO_HOST_BYTES) {
                        Ok(Some(payload)) => {
                            if inbound_tx.blocking_send(payload).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            tracing::debug!("native messaging stdin reached clean EOF");
                            break;
                        }
                        Err(err) => {
                            tracing::warn!("native messaging stdin error: {err}");
                            break;
                        }
                    }
                }
            })?;

        std::thread::Builder::new()
            .name("ferrite-nm-stdout".to_string())
            .spawn(move || {
                let stdout = io::stdout();
                let mut locked = stdout.lock();
                while let Some(payload) = outbound_rx.blocking_recv() {
                    if let Err(err) = write_frame_sync(&mut locked, &payload, MAX_HOST_TO_EXT_BYTES)
                    {
                        tracing::error!("native messaging stdout write error: {err}");
                        break;
                    }
                }
            })?;

        Ok(Self {
            outbound_tx,
            inbound_rx,
        })
    }
}

/// Async Tokio stream bridge for in-memory duplex pipes, socket pairs, and integration tests.
pub struct AsyncFrameBridge {
    pub outbound_tx: mpsc::Sender<Vec<u8>>,
    pub inbound_rx: mpsc::Receiver<Vec<u8>>,
}

impl AsyncFrameBridge {
    pub fn spawn<R, W>(mut reader: R, mut writer: W, inbound_cap: usize, outbound_cap: usize) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

        tokio::spawn(async move {
            loop {
                match read_frame_async(&mut reader, inbound_cap).await {
                    Ok(Some(payload)) => {
                        if inbound_tx.send(payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!("async frame reader stopped: {err}");
                        break;
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Some(payload) = outbound_rx.recv().await {
                if let Err(err) = write_frame_async(&mut writer, &payload, outbound_cap).await {
                    tracing::warn!("async frame writer stopped: {err}");
                    break;
                }
            }
        });

        Self {
            outbound_tx,
            inbound_rx,
        }
    }
}
