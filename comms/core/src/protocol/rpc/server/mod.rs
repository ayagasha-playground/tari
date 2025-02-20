//  Copyright 2021, The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

mod chunking;
use chunking::ChunkedResponseIter;

mod error;
pub use error::RpcServerError;

mod handle;
pub use handle::RpcServerHandle;
use handle::RpcServerRequest;

mod metrics;

pub mod mock;

mod router;
use std::{
    borrow::Cow,
    convert::TryFrom,
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use futures::{future, stream, SinkExt, StreamExt};
use prost::Message;
use router::Router;
use tokio::{sync::mpsc, time};
use tokio_stream::Stream;
use tower::{make::MakeService, Service};
use tracing::{debug, error, instrument, span, trace, warn, Instrument, Level};

use super::{
    body::Body,
    context::{RequestContext, RpcCommsProvider},
    error::HandshakeRejectReason,
    message::{Request, Response, RpcMessageFlags},
    not_found::ProtocolServiceNotFound,
    status::RpcStatus,
    Handshake,
    RPC_MAX_FRAME_SIZE,
};
use crate::{
    bounded_executor::BoundedExecutor,
    framing,
    framing::CanonicalFraming,
    message::MessageExt,
    peer_manager::NodeId,
    proto,
    protocol::{
        rpc::{body::BodyBytes, message::RpcResponse},
        ProtocolEvent,
        ProtocolId,
        ProtocolNotification,
        ProtocolNotificationRx,
    },
    stream_id::StreamId,
    Bytes,
    Substream,
};

const LOG_TARGET: &str = "comms::rpc";

pub trait NamedProtocolService {
    const PROTOCOL_NAME: &'static [u8];

    /// Default implementation that returns a pointer to the static protocol name.
    fn as_protocol_name(&self) -> &'static [u8] {
        Self::PROTOCOL_NAME
    }
}

pub struct RpcServer {
    builder: RpcServerBuilder,
    request_tx: mpsc::Sender<RpcServerRequest>,
    request_rx: mpsc::Receiver<RpcServerRequest>,
}

impl RpcServer {
    pub fn new() -> Self {
        Self::builder().finish()
    }

    pub fn builder() -> RpcServerBuilder {
        RpcServerBuilder::new()
    }

    pub fn add_service<S>(self, service: S) -> Router<S, ProtocolServiceNotFound>
    where
        S: MakeService<
                ProtocolId,
                Request<Bytes>,
                MakeError = RpcServerError,
                Response = Response<Body>,
                Error = RpcStatus,
            > + NamedProtocolService
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        Router::new(self, service)
    }

    pub fn get_handle(&self) -> RpcServerHandle {
        RpcServerHandle::new(self.request_tx.clone())
    }

    pub(super) async fn serve<S, TCommsProvider>(
        self,
        service: S,
        notifications: ProtocolNotificationRx<Substream>,
        comms_provider: TCommsProvider,
    ) -> Result<(), RpcServerError>
    where
        S: MakeService<
                ProtocolId,
                Request<Bytes>,
                MakeError = RpcServerError,
                Response = Response<Body>,
                Error = RpcStatus,
            > + Send
            + 'static,
        S::Service: Send + 'static,
        S::Future: Send + 'static,
        S::Service: Send + 'static,
        <S::Service as Service<Request<Bytes>>>::Future: Send + 'static,
        TCommsProvider: RpcCommsProvider + Clone + Send + 'static,
    {
        PeerRpcServer::new(self.builder, service, notifications, comms_provider, self.request_rx)
            .serve()
            .await
    }
}

impl Default for RpcServer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct RpcServerBuilder {
    maximum_simultaneous_sessions: Option<usize>,
    minimum_client_deadline: Duration,
    handshake_timeout: Duration,
}

impl RpcServerBuilder {
    fn new() -> Self {
        Default::default()
    }

    pub fn with_maximum_simultaneous_sessions(mut self, limit: usize) -> Self {
        self.maximum_simultaneous_sessions = Some(limit);
        self
    }

    pub fn with_unlimited_simultaneous_sessions(mut self) -> Self {
        self.maximum_simultaneous_sessions = None;
        self
    }

    pub fn with_minimum_client_deadline(mut self, deadline: Duration) -> Self {
        self.minimum_client_deadline = deadline;
        self
    }

    pub fn finish(self) -> RpcServer {
        let (request_tx, request_rx) = mpsc::channel(10);
        RpcServer {
            builder: self,
            request_tx,
            request_rx,
        }
    }
}

impl Default for RpcServerBuilder {
    fn default() -> Self {
        Self {
            maximum_simultaneous_sessions: Some(1000),
            minimum_client_deadline: Duration::from_secs(1),
            handshake_timeout: Duration::from_secs(15),
        }
    }
}

pub(super) struct PeerRpcServer<TSvc, TCommsProvider> {
    executor: BoundedExecutor,
    config: RpcServerBuilder,
    service: TSvc,
    protocol_notifications: Option<ProtocolNotificationRx<Substream>>,
    comms_provider: TCommsProvider,
    request_rx: mpsc::Receiver<RpcServerRequest>,
}

impl<TSvc, TCommsProvider> PeerRpcServer<TSvc, TCommsProvider>
where
    TSvc: MakeService<
            ProtocolId,
            Request<Bytes>,
            MakeError = RpcServerError,
            Response = Response<Body>,
            Error = RpcStatus,
        > + Send
        + 'static,
    TSvc::Service: Send + 'static,
    <TSvc::Service as Service<Request<Bytes>>>::Future: Send + 'static,
    TSvc::Future: Send + 'static,
    TCommsProvider: RpcCommsProvider + Clone + Send + 'static,
{
    fn new(
        config: RpcServerBuilder,
        service: TSvc,
        protocol_notifications: ProtocolNotificationRx<Substream>,
        comms_provider: TCommsProvider,
        request_rx: mpsc::Receiver<RpcServerRequest>,
    ) -> Self {
        Self {
            executor: match config.maximum_simultaneous_sessions {
                Some(num) => BoundedExecutor::from_current(num),
                None => BoundedExecutor::allow_maximum(),
            },
            config,
            service,
            protocol_notifications: Some(protocol_notifications),
            comms_provider,
            request_rx,
        }
    }

    pub async fn serve(mut self) -> Result<(), RpcServerError> {
        let mut protocol_notifs = self
            .protocol_notifications
            .take()
            .expect("PeerRpcServer initialized without protocol_notifications");

        loop {
            tokio::select! {
                maybe_notif = protocol_notifs.recv() => {
                    match maybe_notif {
                        Some(notif) => self.handle_protocol_notification(notif).await?,
                        // No more protocol notifications to come, so we're done
                        None => break,
                    }
                }

                Some(req) = self.request_rx.recv() => {
                     self.handle_request(req).await;
                },
            }
        }

        debug!(
            target: LOG_TARGET,
            "Peer RPC server is shut down because the protocol notification stream ended"
        );

        Ok(())
    }

    async fn handle_request(&self, req: RpcServerRequest) {
        use RpcServerRequest::GetNumActiveSessions;
        match req {
            GetNumActiveSessions(reply) => {
                let max_sessions = self
                    .config
                    .maximum_simultaneous_sessions
                    .unwrap_or_else(BoundedExecutor::max_theoretical_tasks);
                let num_active = max_sessions.saturating_sub(self.executor.num_available());
                let _ = reply.send(num_active);
            },
        }
    }

    #[tracing::instrument(name = "rpc::server::new_client_connection", skip(self, notification), err)]
    async fn handle_protocol_notification(
        &mut self,
        notification: ProtocolNotification<Substream>,
    ) -> Result<(), RpcServerError> {
        match notification.event {
            ProtocolEvent::NewInboundSubstream(node_id, substream) => {
                debug!(
                    target: LOG_TARGET,
                    "New client connection for protocol `{}` from peer `{}`",
                    String::from_utf8_lossy(&notification.protocol),
                    node_id
                );

                let framed = framing::canonical(substream, RPC_MAX_FRAME_SIZE);
                match self
                    .try_initiate_service(notification.protocol.clone(), &node_id, framed)
                    .await
                {
                    Ok(_) => {},
                    Err(err @ RpcServerError::HandshakeError(_)) => {
                        debug!(target: LOG_TARGET, "{}", err);
                        metrics::handshake_error_counter(&node_id, &notification.protocol).inc();
                    },
                    Err(err) => {
                        debug!(target: LOG_TARGET, "Unable to spawn RPC service: {}", err);
                    },
                }
            },
        }

        Ok(())
    }

    #[tracing::instrument(name = "rpc::server::try_initiate_service", skip(self, framed), err)]
    async fn try_initiate_service(
        &mut self,
        protocol: ProtocolId,
        node_id: &NodeId,
        mut framed: CanonicalFraming<Substream>,
    ) -> Result<(), RpcServerError> {
        let mut handshake = Handshake::new(&mut framed).with_timeout(self.config.handshake_timeout);

        if !self.executor.can_spawn() {
            debug!(
                target: LOG_TARGET,
                "Rejecting RPC session request for peer `{}` because {}",
                node_id,
                HandshakeRejectReason::NoSessionsAvailable
            );
            handshake
                .reject_with_reason(HandshakeRejectReason::NoSessionsAvailable)
                .await?;
            return Err(RpcServerError::MaximumSessionsReached);
        }

        let service = match self.service.make_service(protocol.clone()).await {
            Ok(s) => s,
            Err(err) => {
                debug!(
                    target: LOG_TARGET,
                    "Rejecting RPC session request for peer `{}` because {}",
                    node_id,
                    HandshakeRejectReason::ProtocolNotSupported
                );
                handshake
                    .reject_with_reason(HandshakeRejectReason::ProtocolNotSupported)
                    .await?;
                return Err(err);
            },
        };

        let version = handshake.perform_server_handshake().await?;
        debug!(
            target: LOG_TARGET,
            "Server negotiated RPC v{} with client node `{}`", version, node_id
        );

        let service = ActivePeerRpcService::new(
            self.config.clone(),
            protocol,
            node_id.clone(),
            service,
            framed,
            self.comms_provider.clone(),
        );

        let node_id = node_id.clone();
        self.executor
            .try_spawn(async move {
                let num_sessions = metrics::num_sessions(&node_id, &service.protocol);
                num_sessions.inc();
                service.start().await;
                num_sessions.dec();
            })
            .map_err(|_| RpcServerError::MaximumSessionsReached)?;

        Ok(())
    }
}

struct ActivePeerRpcService<TSvc, TCommsProvider> {
    config: RpcServerBuilder,
    protocol: ProtocolId,
    node_id: NodeId,
    service: TSvc,
    framed: CanonicalFraming<Substream>,
    comms_provider: TCommsProvider,
    logging_context_string: Arc<String>,
}

impl<TSvc, TCommsProvider> ActivePeerRpcService<TSvc, TCommsProvider>
where
    TSvc: Service<Request<Bytes>, Response = Response<Body>, Error = RpcStatus>,
    TCommsProvider: RpcCommsProvider + Send + Clone + 'static,
{
    pub(self) fn new(
        config: RpcServerBuilder,
        protocol: ProtocolId,
        node_id: NodeId,
        service: TSvc,
        framed: CanonicalFraming<Substream>,
        comms_provider: TCommsProvider,
    ) -> Self {
        Self {
            logging_context_string: Arc::new(format!(
                "stream_id: {}, peer: {}, protocol: {}",
                framed.stream_id(),
                node_id,
                String::from_utf8_lossy(&protocol)
            )),

            config,
            protocol,
            node_id,
            service,
            framed,
            comms_provider,
        }
    }

    async fn start(mut self) {
        debug!(
            target: LOG_TARGET,
            "({}) Rpc server started.", self.logging_context_string,
        );
        if let Err(err) = self.run().await {
            metrics::error_counter(&self.node_id, &self.protocol, &err).inc();
            error!(
                target: LOG_TARGET,
                "({}) Rpc server exited with an error: {}", self.logging_context_string, err
            );
        }
    }

    async fn run(&mut self) -> Result<(), RpcServerError> {
        let request_bytes = metrics::inbound_requests_bytes(&self.node_id, &self.protocol);
        while let Some(result) = self.framed.next().await {
            match result {
                Ok(frame) => {
                    let start = Instant::now();
                    request_bytes.observe(frame.len() as f64);
                    if let Err(err) = self.handle_request(frame.freeze()).await {
                        if let Err(err) = self.framed.close().await {
                            error!(
                                target: LOG_TARGET,
                                "({}) Failed to close substream after socket error: {}",
                                self.logging_context_string,
                                err
                            );
                        }
                        error!(
                            target: LOG_TARGET,
                            "(peer: {}, protocol: {}) Failed to handle request: {}",
                            self.node_id,
                            self.protocol_name(),
                            err
                        );
                        return Err(err);
                    }
                    let elapsed = start.elapsed();
                    debug!(
                        target: LOG_TARGET,
                        "({}) RPC request completed in {:.0?}{}",
                        self.logging_context_string,
                        elapsed,
                        if elapsed.as_secs() > 5 { " (LONG REQUEST)" } else { "" }
                    );
                },
                Err(err) => {
                    if let Err(err) = self.framed.close().await {
                        error!(
                            target: LOG_TARGET,
                            "({}) Failed to close substream after socket error: {}", self.logging_context_string, err
                        );
                    }
                    return Err(err.into());
                },
            }
        }

        self.framed.close().await?;
        Ok(())
    }

    #[instrument(name = "rpc::server::handle_req", skip(self, request), err, fields(request_size = request.len()))]
    async fn handle_request(&mut self, mut request: Bytes) -> Result<(), RpcServerError> {
        let decoded_msg = proto::rpc::RpcRequest::decode(&mut request)?;

        let request_id = decoded_msg.request_id;
        let method = decoded_msg.method.into();
        let deadline = Duration::from_secs(decoded_msg.deadline);

        // The client side deadline MUST be greater or equal to the minimum_client_deadline
        if deadline < self.config.minimum_client_deadline {
            debug!(
                target: LOG_TARGET,
                "({}) Client has an invalid deadline. {}", self.logging_context_string, decoded_msg
            );
            // Let the client know that they have disobeyed the spec
            let status = RpcStatus::bad_request(&format!(
                "Invalid deadline ({:.0?}). The deadline MUST be greater than {:.0?}.",
                self.node_id, deadline,
            ));
            let bad_request = proto::rpc::RpcResponse {
                request_id,
                status: status.as_code(),
                flags: RpcMessageFlags::FIN.bits().into(),
                payload: status.to_details_bytes(),
            };
            metrics::status_error_counter(&self.node_id, &self.protocol, status.as_status_code()).inc();
            self.framed.send(bad_request.to_encoded_bytes().into()).await?;
            return Ok(());
        }

        let msg_flags = RpcMessageFlags::from_bits_truncate(u8::try_from(decoded_msg.flags).unwrap());

        if msg_flags.contains(RpcMessageFlags::FIN) {
            debug!(target: LOG_TARGET, "({}) Client sent FIN.", self.logging_context_string);
            return Ok(());
        }
        if msg_flags.contains(RpcMessageFlags::ACK) {
            debug!(
                target: LOG_TARGET,
                "({}) sending ACK response.", self.logging_context_string
            );
            let ack = proto::rpc::RpcResponse {
                request_id,
                status: RpcStatus::ok().as_code(),
                flags: RpcMessageFlags::ACK.bits().into(),
                ..Default::default()
            };
            self.framed.send(ack.to_encoded_bytes().into()).await?;
            return Ok(());
        }

        debug!(
            target: LOG_TARGET,
            "({}) Request: {}", self.logging_context_string, decoded_msg
        );

        let req = Request::with_context(
            self.create_request_context(request_id),
            method,
            decoded_msg.payload.into(),
        );

        let service_call = log_timing(
            self.logging_context_string.clone(),
            request_id,
            "service call",
            self.service.call(req),
        );
        let service_result = time::timeout(deadline, service_call).await;
        let service_result = match service_result {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    target: LOG_TARGET,
                    "{} RPC service was not able to complete within the deadline ({:.0?}). Request aborted",
                    self.logging_context_string,
                    deadline,
                );

                metrics::error_counter(
                    &self.node_id,
                    &self.protocol,
                    &RpcServerError::ServiceCallExceededDeadline,
                )
                .inc();
                return Ok(());
            },
        };

        match service_result {
            Ok(body) => {
                self.process_body(request_id, deadline, body).await?;
            },
            Err(err) => {
                error!(
                    target: LOG_TARGET,
                    "{} Service returned an error: {}", self.logging_context_string, err
                );
                let resp = proto::rpc::RpcResponse {
                    request_id,
                    status: err.as_code(),
                    flags: RpcMessageFlags::FIN.bits().into(),
                    payload: err.to_details_bytes(),
                };

                metrics::status_error_counter(&self.node_id, &self.protocol, err.as_status_code()).inc();
                self.framed.send(resp.to_encoded_bytes().into()).await?;
            },
        }

        Ok(())
    }

    fn protocol_name(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.protocol)
    }

    async fn process_body(
        &mut self,
        request_id: u32,
        deadline: Duration,
        body: Response<Body>,
    ) -> Result<(), RpcServerError> {
        let response_bytes = metrics::outbound_response_bytes(&self.node_id, &self.protocol);
        trace!(target: LOG_TARGET, "Service call succeeded");

        let node_id = self.node_id.clone();
        let protocol = self.protocol.clone();
        let mut stream = body
            .into_message()
            .map(|result| into_response(request_id, result))
            .flat_map(move |message| {
                if !message.status.is_ok() {
                    metrics::status_error_counter(&node_id, &protocol, message.status).inc();
                }
                stream::iter(ChunkedResponseIter::new(message))
            })
            .map(|resp| Bytes::from(resp.to_encoded_bytes()));

        loop {
            // Check if the client interrupted the outgoing stream
            if let Err(err) = self.check_interruptions().await {
                match err {
                    err @ RpcServerError::ClientInterruptedStream => {
                        debug!(target: LOG_TARGET, "Stream was interrupted: {}", err);
                        break;
                    },
                    err => {
                        error!(target: LOG_TARGET, "Stream was interrupted: {}", err);
                        return Err(err);
                    },
                }
            }

            let next_item = log_timing(
                self.logging_context_string.clone(),
                request_id,
                "message read",
                stream.next(),
            );
            match time::timeout(deadline, next_item).await {
                Ok(Some(msg)) => {
                    response_bytes.observe(msg.len() as f64);
                    debug!(
                        target: LOG_TARGET,
                        "({}) Sending body len = {}",
                        self.logging_context_string,
                        msg.len()
                    );

                    self.framed.send(msg).await?;
                },
                Ok(None) => {
                    debug!(target: LOG_TARGET, "{} Request complete", self.logging_context_string,);
                    break;
                },
                Err(_) => {
                    debug!(
                        target: LOG_TARGET,
                        "({}) Failed to return result within client deadline ({:.0?})",
                        self.logging_context_string,
                        deadline
                    );

                    metrics::error_counter(
                        &self.node_id,
                        &self.protocol,
                        &RpcServerError::ReadStreamExceededDeadline,
                    )
                    .inc();
                    break;
                },
            }
        } // end loop
        Ok(())
    }

    async fn check_interruptions(&mut self) -> Result<(), RpcServerError> {
        let check = future::poll_fn(|cx| match Pin::new(&mut self.framed).poll_next(cx) {
            Poll::Ready(Some(Ok(mut msg))) => {
                let decoded_msg = match proto::rpc::RpcRequest::decode(&mut msg) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!(target: LOG_TARGET, "Client send MALFORMED response: {}", err);
                        return Poll::Ready(Some(RpcServerError::UnexpectedIncomingMessageMalformed));
                    },
                };
                let msg_flags = RpcMessageFlags::from_bits_truncate(u8::try_from(decoded_msg.flags).unwrap());
                if msg_flags.is_fin() {
                    Poll::Ready(Some(RpcServerError::ClientInterruptedStream))
                } else {
                    Poll::Ready(Some(RpcServerError::UnexpectedIncomingMessage(decoded_msg)))
                }
            },
            Poll::Ready(Some(Err(err))) if err.kind() == io::ErrorKind::WouldBlock => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(RpcServerError::from(err))),
            Poll::Ready(None) => Poll::Ready(Some(RpcServerError::StreamClosedByRemote)),
            Poll::Pending => Poll::Ready(None),
        })
        .await;
        match check {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn create_request_context(&self, request_id: u32) -> RequestContext {
        RequestContext::new(request_id, self.node_id.clone(), Box::new(self.comms_provider.clone()))
    }
}

async fn log_timing<R, F: Future<Output = R>>(context_str: Arc<String>, request_id: u32, tag: &str, fut: F) -> R {
    let t = Instant::now();
    let span = span!(Level::TRACE, "rpc::internal::timing", request_id, tag);
    let ret = fut.instrument(span).await;
    let elapsed = t.elapsed();
    trace!(
        target: LOG_TARGET,
        "({}) RPC TIMING(REQ_ID={}): '{}' took {:.2}s{}",
        context_str,
        request_id,
        tag,
        elapsed.as_secs_f32(),
        if elapsed.as_secs() >= 5 { " (SLOW)" } else { "" }
    );
    ret
}

#[allow(clippy::cognitive_complexity)]
fn into_response(request_id: u32, result: Result<BodyBytes, RpcStatus>) -> RpcResponse {
    match result {
        Ok(msg) => {
            trace!(target: LOG_TARGET, "Sending body len = {}", msg.len());
            let mut flags = RpcMessageFlags::empty();
            if msg.is_finished() {
                flags |= RpcMessageFlags::FIN;
            }
            RpcResponse {
                request_id,
                status: RpcStatus::ok().as_status_code(),
                flags,
                payload: msg.into_bytes().unwrap_or_else(Bytes::new),
            }
        },
        Err(err) => {
            debug!(target: LOG_TARGET, "Body contained an error: {}", err);
            RpcResponse {
                request_id,
                status: err.as_status_code(),
                flags: RpcMessageFlags::FIN,
                payload: Bytes::from(err.to_details_bytes()),
            }
        },
    }
}
