use std::{net::SocketAddr, sync::Arc};

use async_channel::{unbounded, Receiver, Sender};
use postage::stream::Stream;
use stratum_apps::{
    custom_mutex::Mutex,
    key_utils::Secp256k1PublicKey,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        codec_sv2::HandshakeRole,
        framing_sv2,
        handlers_sv2::HandleCommonMessagesFromServerAsync,
        noise_sv2::Initiator,
        parsers_sv2::{AnyMessage, JobDeclaration},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{protocol_message_type, MessageType},
        types::{Message, Sv2Frame},
    },
};
use tokio::{net::TcpStream, sync::mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    config::ConfigJDCMode,
    error::{self, JDCError, JDCErrorKind, JDCResult},
    io_task::spawn_io_tasks,
    status::{handle_error, Status, StatusSender},
    utils::{get_setup_connection_message_jds, ShutdownMessage},
};

mod message_handler;

/// Shared state for Job Declarator
pub struct JobDeclaratorData;

/// Holds all channels required for Job Declarator communication.
#[derive(Clone)]
pub struct JobDeclaratorChannel {
    channel_manager_sender: Sender<JobDeclaration<'static>>,
    channel_manager_receiver: Receiver<JobDeclaration<'static>>,
    jds_sender: Sender<Sv2Frame>,
    jds_receiver: Receiver<Sv2Frame>,
}

/// Manages the lifecycle and communication with a Job Declarator (JDS)
#[allow(warnings)]
#[derive(Clone)]
pub struct JobDeclarator {
    /// Internal state
    job_declarator_data: Arc<Mutex<JobDeclaratorData>>,
    /// Messaging channels to/from the channel manager and JD.
    job_declarator_channel: JobDeclaratorChannel,
    /// Socket address of the Job Declarator server.
    socket_address: SocketAddr,
    /// Config JDC mode
    mode: ConfigJDCMode,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclarator {
    /// Creates a new JobDeclarator instance by connecting and performing a Noise handshake.
    ///
    /// - Establishes TCP connection.
    /// - Performs SV2 Noise handshake.
    /// - Spawns background IO tasks for reading/writing frames.
    pub async fn new(
        upstreams: &(SocketAddr, SocketAddr, Secp256k1PublicKey, bool),
        channel_manager_sender: Sender<JobDeclaration<'static>>,
        channel_manager_receiver: Receiver<JobDeclaration<'static>>,
        notify_shutdown: postage::broadcast::Sender<ShutdownMessage>,
        mode: ConfigJDCMode,
        task_manager: Arc<TaskManager>,
        status_sender: Sender<Status>,
    ) -> JDCResult<Self, error::JobDeclarator> {
        let (_, addr, pubkey, _) = upstreams;
        info!("Connecting to JD Server at {addr}");
        let stream = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            TcpStream::connect(addr),
        )
        .await
        .map_err(JDCError::fallback)?
        .map_err(JDCError::fallback)?;
        info!("Connection established with JD Server at {addr} in mode: {mode:?}");
        let initiator = Initiator::from_raw_k(pubkey.into_bytes()).map_err(JDCError::fallback)?;
        let (noise_stream_reader, noise_stream_writer) =
            NoiseTcpStream::<Message>::new(stream, HandshakeRole::Initiator(initiator))
                .await
                .map_err(JDCError::fallback)?
                .into_split();

        let status_sender = StatusSender::JobDeclarator(status_sender);
        let (inbound_tx, inbound_rx) = unbounded::<Sv2Frame>();
        let (outbound_tx, outbound_rx) = unbounded::<Sv2Frame>();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            notify_shutdown,
            status_sender,
        );
        let job_declarator_data = Arc::new(Mutex::new(JobDeclaratorData));
        let job_declarator_channel = JobDeclaratorChannel {
            channel_manager_receiver,
            channel_manager_sender,
            jds_sender: outbound_tx,
            jds_receiver: inbound_rx,
        };
        Ok(JobDeclarator {
            job_declarator_channel,
            job_declarator_data,
            socket_address: *addr,
            mode,
        })
    }

    /// Starts the JobDeclarator message loop.
    ///
    /// - Waits for shutdown signals.
    /// - Handles incoming messages from Job Declarator and Channel Manager.
    /// - Cleans up on termination.
    pub async fn start(
        mut self,
        notify_shutdown: postage::broadcast::Sender<ShutdownMessage>,
        shutdown_complete_tx: mpsc::Sender<()>,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) {
        let status_sender = StatusSender::JobDeclarator(status_sender);
        let mut shutdown_rx = notify_shutdown.subscribe();

        if let Err(e) = self.setup_connection().await {
            handle_error(&status_sender, e).await;
            return;
        }

        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Some(ShutdownMessage::ShutdownAll) => {
                                info!("Job Declarator: received shutdown signal.");
                                break;
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdownFallback(_)) => {
                                info!("Job Declarator: Received Job declarator shutdown.");
                                break;
                            }
                            Some(ShutdownMessage::UpstreamShutdownFallback(_)) => {
                                info!("Job Declarator: Received Upstream shutdown.");
                                break;
                            }
                            Some(ShutdownMessage::UpstreamShutdown(tx)) => {
                                info!("Job declarator shutdown requested");
                                drop(tx);
                                break;
                            }
                            Some(ShutdownMessage::JobDeclaratorShutdown(tx)) => {
                                info!("Job declarator shutdown requested");
                                drop(tx);
                                break;
                            }
                            None => {
                                warn!("Job Declarator: shutdown channel closed unexpectedly");
                                break;
                            }
                            _ => {}
                        }
                    }
                    res = self_clone_1.handle_job_declarator_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Job Declarator message handling failed");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Channel Manager message handling failed");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    },
                }
            }
            drop(shutdown_complete_tx);
            warn!("JobDeclarator: unified message loop exited.");
        });
    }

    /// Performs SV2 setup connection handshake with Job Declarator server.
    ///
    /// - Sends `SetupConnection` message.
    /// - Waits for and validates server response.
    /// - Completes SV2 protocol handshake.
    pub async fn setup_connection(&mut self) -> JDCResult<(), error::JobDeclarator> {
        info!("Sending SetupConnection to JDS at {}", self.socket_address);

        let setup_connection = get_setup_connection_message_jds(&self.socket_address, &self.mode);
        let sv2_frame: Sv2Frame = Message::Common(setup_connection.into())
            .try_into()
            .map_err(|e| {
                error!(error=?e, "Failed to serialize SetupConnection message.");
                JDCError::shutdown(e)
            })?;

        if let Err(e) = self.job_declarator_channel.jds_sender.send(sv2_frame).await {
            error!(error=?e, "Failed to send SetupConnection frame.");
            return Err(JDCError::fallback(JDCErrorKind::ChannelErrorSender));
        }
        debug!("SetupConnection frame sent successfully.");

        let mut incoming = self
            .job_declarator_channel
            .jds_receiver
            .recv()
            .await
            .map_err(|e| {
                error!(error=?e, "No handshake response received from Job declarator.");
                JDCError::fallback(JDCErrorKind::ChannelErrorSender)
            })?;

        let header = incoming.get_header().ok_or_else(|| {
            error!("Handshake frame missing header.");
            JDCError::fallback(framing_sv2::Error::MissingHeader)
        })?;

        debug!(ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Processing handshake response.");

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;

        info!("Job declarator: SV2 handshake completed successfully.");
        Ok(())
    }

    // Handles messages coming from the Channel Manager and forwards them to the Job Declarator.
    async fn handle_channel_manager_message(&self) -> JDCResult<(), error::JobDeclarator> {
        match self
            .job_declarator_channel
            .channel_manager_receiver
            .recv()
            .await
        {
            Ok(msg) => {
                debug!("Forwarding message from channel manager to JDS.");
                let message = AnyMessage::JobDeclaration(msg);
                let sv2_frame: Sv2Frame = message.try_into().map_err(JDCError::shutdown)?;
                self.job_declarator_channel
                    .jds_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!("Failed to send message to outbound channel: {:?}", e);
                        JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            Err(e) => {
                warn!("Channel manager receiver closed or errored: {:?}", e);
            }
        }
        Ok(())
    }

    // Handles messages received from the Job Declarator.
    //
    // - Forwards `JobDeclaration` messages to Channel Manager.
    // - Processes `Common` messages via handler.
    // - Rejects unsupported message types.
    async fn handle_job_declarator_message(&mut self) -> JDCResult<(), error::JobDeclarator> {
        let mut sv2_frame = self
            .job_declarator_channel
            .jds_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;

        debug!("Received SV2 frame from JDS.");
        let header = sv2_frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            JDCError::fallback(framing_sv2::Error::MissingHeader)
        })?;
        let message_type = header.msg_type();
        let extension_type = header.ext_type();

        match protocol_message_type(extension_type, message_type) {
            MessageType::Common => {
                info!(ext_type = ?extension_type, msg_type = ?message_type, "Handling common message from Upstream.");
                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::JobDeclaration => {
                let message = JobDeclaration::try_from((message_type, sv2_frame.payload()))
                    .map_err(JDCError::fallback)?
                    .into_static();
                self.job_declarator_channel
                    .channel_manager_sender
                    .send(message)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send Job declaration message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!("Received unsupported message type from Job declarator: {message_type}");
            }
        }

        Ok(())
    }
}
