use std::sync::Arc;

use async_channel::{Receiver, Sender};
use postage::stream::Stream;
use stratum_apps::{
    network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf},
    stratum_core::framing_sv2::framing::Frame,
    task_manager::TaskManager,
    utils::types::{Message, Sv2Frame},
};
use tracing::{error, trace, warn, Instrument as _};

use crate::{
    status::{StatusSender, StatusType},
    utils::ShutdownMessage,
};

/// Spawns async reader and writer tasks for handling framed I/O with shutdown support.
#[track_caller]
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
pub fn spawn_io_tasks(
    task_manager: Arc<TaskManager>,
    mut reader: NoiseTcpReadHalf<Message>,
    mut writer: NoiseTcpWriteHalf<Message>,
    outbound_rx: Receiver<Sv2Frame>,
    inbound_tx: Sender<Sv2Frame>,
    notify_shutdown: postage::broadcast::Sender<ShutdownMessage>,
    status_sender: StatusSender,
) {
    let caller = std::panic::Location::caller();
    let inbound_tx_clone = inbound_tx.clone();
    let outbound_rx_clone = outbound_rx.clone();
    {
        let mut shutdown_rx = notify_shutdown.subscribe();
        let status_sender = status_sender.clone();
        let status_type: StatusType = StatusType::from(&status_sender);

        task_manager.spawn(async move {
            trace!("Reader task started");
            loop {
                tokio::select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Some(ShutdownMessage::ShutdownAll) => {
                                trace!("Received global shutdown");
                                inbound_tx.close();
                                break;
                            }
                            Some(ShutdownMessage::DownstreamShutdown(down_id))  if matches!(status_type, StatusType::Downstream(id) if id == down_id) => {
                                trace!(down_id, "Received downstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    inbound_tx.close();
                                    break;
                                }
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdownFallback(_)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received job declarator shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    inbound_tx.close();
                                    break;
                                }
                            }
                            Some(ShutdownMessage::UpstreamShutdownFallback(_)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    inbound_tx.close();
                                    break;
                                }
                            }

                            Some(ShutdownMessage::UpstreamShutdown(tx)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    inbound_tx.close();
                                    break;
                                }
                                drop(tx);
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdown(tx)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    inbound_tx.close();
                                    break;
                                }
                                drop(tx);
                            }
                            _ => {}
                        }
                    }
                    res = reader.read_frame() => {
                        match res {
                            Ok(frame) => {
                                match frame {
                                    Frame::HandShake(frame) => {
                                        error!(?frame, "Received handshake frame");
                                        drop(frame);
                                        break;
                                    },
                                    Frame::Sv2(sv2_frame) => {
                                        trace!("Received inbound frame");
                                        if let Err(e) = inbound_tx.send(sv2_frame).await {
                                            inbound_tx.close();
                                            error!(error=?e, "Failed to forward inbound frame");
                                            break;
                                        }
                                    },
                                }
                            }
                            Err(e) => {
                                error!(error=?e, "Reader error");
                                inbound_tx.close();
                                break;
                            }
                        }
                    }
                }
            }
            inbound_tx.close();
            outbound_rx_clone.close();
            drop(inbound_tx);
            drop(outbound_rx_clone);
            warn!("Reader task exited.");
        }.instrument(tracing::trace_span!(
            "reader_task",
            spawned_at = %format!("{}:{}", caller.file(), caller.line())
        )));
    }

    {
        let mut shutdown_rx = notify_shutdown.subscribe();
        let status_type: StatusType = StatusType::from(&status_sender);

        task_manager.spawn(async move {
            trace!("Writer task started");
            loop {
                tokio::select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Some(ShutdownMessage::ShutdownAll) => {
                                trace!("Received global shutdown");
                                outbound_rx.close();
                                break;
                            }
                            Some(ShutdownMessage::DownstreamShutdown(down_id))  if matches!(status_type, StatusType::Downstream(id) if id == down_id) => {
                                trace!(down_id, "Received downstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    outbound_rx.close();
                                    break;
                                }
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdownFallback(_)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received job declarator shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    outbound_rx.close();
                                    break;
                                }
                            }
                            Some(ShutdownMessage::UpstreamShutdownFallback(_)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    outbound_rx.close();
                                    break;
                                }
                            }
                            Some(ShutdownMessage::UpstreamShutdown(tx)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    outbound_rx.close();
                                    break;
                                }
                                drop(tx);
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdown(tx)) if !matches!(status_type, StatusType::TemplateReceiver) => {
                                trace!("Received upstream shutdown");
                                if status_type != StatusType::TemplateReceiver {
                                    outbound_rx.close();
                                    break;
                                }
                                drop(tx);
                            }
                            _ => {}
                        }
                    }
                    res = outbound_rx.recv() => {
                        match res {
                            Ok(frame) => {
                                trace!("Sending outbound frame");
                                if let Err(e) = writer.write_frame(frame.into()).await {
                                    error!(error=?e, "Writer error");
                                    outbound_rx.close();
                                    break;
                                }
                            }
                            Err(_) => {
                                outbound_rx.close();
                                warn!("Outbound channel closed");
                                break;
                            }
                        }
                    }
                }
            }
            outbound_rx.close();
            inbound_tx_clone.close();
            drop(outbound_rx);
            drop(inbound_tx_clone);
            warn!("Writer task exited.");
        }.instrument(tracing::trace_span!(
            "writer_task",
            spawned_at = %format!("{}:{}", caller.file(), caller.line())
        )));
    }
}
