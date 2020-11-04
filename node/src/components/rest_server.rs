//! API server
//!
//! The API server provides clients with two types of service: a JSON-RPC API for querying state and
//! sending commands to the node, and an event-stream returning Server-Sent Events (SSEs) holding
//! JSON-encoded data.
//!
//! The actual server is run in backgrounded tasks.   RPCs requests are translated into reactor
//! requests to various components.
//!
//! This module currently provides both halves of what is required for an API server: An abstract
//! API Server that handles API requests and an external service endpoint based on HTTP.
//!
//! For the list of supported RPCs and SSEs, see
//! https://github.com/CasperLabs/ceps/blob/master/text/0009-client-api.md#rpcs

mod config;
mod event;
mod filters;
mod http_server;

use std::{convert::Infallible, fmt::Debug};

use datasize::DataSize;
use futures::{future::BoxFuture, join, FutureExt};
use tokio::{sync::oneshot, task::JoinHandle};
use tracing::{debug, error, warn};

use super::Component;
use crate::{
    components::storage::Storage,
    effect::{
        requests::{ChainspecLoaderRequest, MetricsRequest, NetworkInfoRequest, StorageRequest},
        EffectBuilder, EffectExt, Effects,
    },
    reactor::Finalize,
    small_network::NodeId,
    types::{CryptoRngCore, StatusFeed},
};

use crate::effect::requests::RestRequest;
pub use config::Config;
pub(crate) use event::Event;

/// A helper trait whose bounds represent the requirements for a reactor event that `run_server` can
/// work with.
pub trait ReactorEventT:
    From<Event>
    + From<RestRequest<NodeId>>
    + From<NetworkInfoRequest<NodeId>>
    + From<StorageRequest<Storage>>
    + From<ChainspecLoaderRequest>
    + From<MetricsRequest>
    + Send
{
}

impl<REv> ReactorEventT for REv where
    REv: From<Event>
        + From<RestRequest<NodeId>>
        + From<NetworkInfoRequest<NodeId>>
        + From<StorageRequest<Storage>>
        + From<ChainspecLoaderRequest>
        + From<MetricsRequest>
        + Send
        + 'static
{
}

#[derive(DataSize, Debug)]
pub(crate) struct RestServer {
    /// When the message is sent, it signals the server loop to exit cleanly.
    shutdown_sender: oneshot::Sender<()>,
    /// The task handle which will only join once the server loop has exited.
    server_join_handle: Option<JoinHandle<()>>,
}

impl RestServer {
    pub(crate) fn new<REv>(config: Config, effect_builder: EffectBuilder<REv>) -> Self
    where
        REv: ReactorEventT,
    {
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();

        let server_join_handle =
            tokio::spawn(http_server::run(config, effect_builder, shutdown_receiver));

        RestServer {
            shutdown_sender,
            server_join_handle: Some(server_join_handle),
        }
    }
}

impl<REv> Component<REv> for RestServer
where
    REv: ReactorEventT,
{
    type Event = Event;
    type ConstructionError = Infallible;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut dyn CryptoRngCore,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::RestRequest(RestRequest::GetStatus { responder }) => async move {
                debug!("REST server handle_event get_status");
                let (last_added_block, peers, chainspec_info) = join!(
                    effect_builder.get_highest_block(),
                    effect_builder.network_peers(),
                    effect_builder.get_chainspec_info()
                );
                let status_feed = StatusFeed::new(last_added_block, peers, chainspec_info);
                responder.respond(status_feed).await;
            }
            .ignore(),
            Event::RestRequest(RestRequest::GetMetrics { responder }) => {
                debug!("REST server handle_event get_metrics");
                effect_builder
                    .get_metrics()
                    .event(move |text| Event::GetMetricsResult {
                        text,
                        main_responder: responder,
                    })
            }
            Event::GetMetricsResult {
                text,
                main_responder,
            } => {
                debug!("REST server handle_event get_metrics_result");
                main_responder.respond(text).ignore()
            }
        }
    }
}

impl Finalize for RestServer {
    fn finalize(mut self) -> BoxFuture<'static, ()> {
        async {
            let _ = self.shutdown_sender.send(());

            // Wait for the server to exit cleanly.
            if let Some(join_handle) = self.server_join_handle.take() {
                match join_handle.await {
                    Ok(_) => debug!("rest server exited cleanly"),
                    Err(error) => error!(%error, "could not join rest server task cleanly"),
                }
            } else {
                warn!("rest server shutdown while already shut down")
            }
        }
        .boxed()
    }
}
