use std::marker::PhantomData;
#[cfg(feature = "rust-tls")]
use std::net;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::mpsc::channel as std_channel;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::channel;

use crate::authenticators::SaslAuthenticatorProvider;
use crate::cluster::connection_manager::ConnectionManager;
#[cfg(feature = "rust-tls")]
use crate::cluster::rustls_connection_manager::RustlsConnectionManager;
use crate::cluster::tcp_connection_manager::TcpConnectionManager;
#[cfg(feature = "rust-tls")]
use crate::cluster::ClusterRustlsConfig;
#[cfg(feature = "rust-tls")]
use crate::cluster::NodeRustlsConfigBuilder;
use crate::cluster::{ClusterTcpConfig, GenericClusterConfig, GetRetryPolicy, KeyspaceHolder};
use crate::cluster::{NodeTcpConfigBuilder, SessionPager};
use crate::compression::Compression;
use crate::error;
use crate::events::{new_listener, EventStream, EventStreamNonBlocking, Listener};
use crate::frame::events::SimpleServerEvent;
use crate::frame::frame_result::BodyResResultPrepared;
use crate::frame::Frame;
use crate::load_balancing::LoadBalancingStrategy;
use crate::query::utils::{prepare_flags, send_frame};
use crate::query::{
    PreparedQuery, Query, QueryBatch, QueryParams, QueryParamsBuilder, QueryValues,
};
use crate::retry::{
    DefaultRetryPolicy, ExponentialReconnectionPolicy, NeverReconnectionPolicy, ReconnectionPolicy,
    RetryPolicy,
};
#[cfg(feature = "rust-tls")]
use crate::transport::TransportRustls;
use crate::transport::{CdrsTransport, TransportTcp};

static NEVER_RECONNECTION_POLICY: NeverReconnectionPolicy = NeverReconnectionPolicy;

pub const DEFAULT_TRANSPORT_BUFFER_SIZE: usize = 1024;

/// CDRS session that holds a pool of connections to nodes.
pub struct Session<
    T: CdrsTransport + Send + Sync + 'static,
    CM: ConnectionManager<T>,
    LB: LoadBalancingStrategy<CM> + Send + Sync,
> {
    load_balancing: LB,
    compression: Compression,
    transport_buffer_size: usize,
    tcp_nodelay: bool,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    _transport: PhantomData<T>,
    _connection_manager: PhantomData<CM>,
}

impl<
        'a,
        T: CdrsTransport + Send + Sync + 'static,
        CM: ConnectionManager<T>,
        LB: LoadBalancingStrategy<CM> + Send + Sync,
    > Session<T, CM, LB>
{
    /// Basing on current session returns new `SessionPager` that can be used
    /// for performing paged queries.
    pub fn paged(&'a self, page_size: i32) -> SessionPager<'a, T, CM, LB> {
        SessionPager::new(self, page_size)
    }

    /// Executes given prepared query with query parameters and optional tracing, and warnings.
    pub async fn exec_with_params_tw(
        &self,
        prepared: &PreparedQuery,
        query_parameters: QueryParams,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let flags = prepare_flags(with_tracing, with_warnings);
        let options_frame = Frame::new_req_execute(
            prepared
                .id
                .read()
                .expect("Cannot read prepared query id!")
                .deref(),
            &query_parameters,
            flags,
        );

        let mut result = send_frame(self, options_frame, query_parameters.is_idempotent).await;

        if let Err(error::Error::Server(error)) = &result {
            // if query is unprepared
            if error.error_code == 0x2500 {
                if let Ok(new) = self.prepare_raw(&prepared.query).await {
                    *prepared
                        .id
                        .write()
                        .expect("Cannot write prepared query id!") = new.id.clone();
                    let flags = prepare_flags(with_tracing, with_warnings);
                    let options_frame = Frame::new_req_execute(&new.id, &query_parameters, flags);
                    result = send_frame(self, options_frame, query_parameters.is_idempotent).await;
                }
            }
        }
        result
    }

    /// Executes given prepared query with query parameters.
    pub async fn exec_with_params(
        &self,
        prepared: &PreparedQuery,
        query_parameters: QueryParams,
    ) -> error::Result<Frame> {
        self.exec_with_params_tw(prepared, query_parameters, false, false)
            .await
    }

    /// Executes given prepared query with query values and optional tracing, and warnings.
    pub async fn exec_with_values_tw<V: Into<QueryValues> + Sync + Send>(
        &self,
        prepared: &PreparedQuery,
        values: V,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let query_params_builder = QueryParamsBuilder::new();
        let query_params = query_params_builder.values(values.into()).finalize();
        self.exec_with_params_tw(prepared, query_params, with_tracing, with_warnings)
            .await
    }

    /// Executes given prepared query with query values.
    pub async fn exec_with_values<V: Into<QueryValues> + Sync + Send>(
        &self,
        prepared: &PreparedQuery,
        values: V,
    ) -> error::Result<Frame> {
        self.exec_with_values_tw(prepared, values, false, false)
            .await
    }

    /// Executes given prepared query with optional tracing and warnings.
    pub async fn exec_tw(
        &self,
        prepared: &PreparedQuery,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let query_params = QueryParamsBuilder::new().finalize();
        self.exec_with_params_tw(prepared, query_params, with_tracing, with_warnings)
            .await
    }

    /// Executes given prepared query.
    pub async fn exec(&self, prepared: &PreparedQuery) -> error::Result<Frame>
    where
        Self: Sync,
    {
        self.exec_tw(prepared, false, false).await
    }

    /// Prepares a query for execution. Along with query itself, the
    /// method takes `with_tracing` and `with_warnings` flags to get
    /// tracing information and warnings. Returns the raw prepared
    /// query result.
    pub async fn prepare_raw_tw<Q: ToString + Sync + Send>(
        &self,
        query: Q,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<BodyResResultPrepared> {
        let flags = prepare_flags(with_tracing, with_warnings);

        let query_frame = Frame::new_req_prepare(query.to_string(), flags);

        send_frame(self, query_frame, false)
            .await
            .and_then(|response| response.body())
            .and_then(|body| {
                body.into_prepared()
                    .ok_or_else(|| "CDRS BUG: cannot convert frame into prepared".into())
            })
    }

    /// Prepares query without additional tracing information and warnings.
    /// Returns the raw prepared query result.
    pub async fn prepare_raw<Q: ToString + Sync + Send>(
        &self,
        query: Q,
    ) -> error::Result<BodyResResultPrepared> {
        self.prepare_raw_tw(query, false, false).await
    }

    /// Prepares a query for execution. Along with query itself,
    /// the method takes `with_tracing` and `with_warnings` flags
    /// to get tracing information and warnings. Returns the prepared
    /// query.
    pub async fn prepare_tw<Q: ToString + Sync + Send>(
        &self,
        query: Q,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<PreparedQuery> {
        let s = query.to_string();
        self.prepare_raw_tw(query, with_tracing, with_warnings)
            .await
            .map(|x| PreparedQuery {
                id: RwLock::new(x.id),
                query: s,
            })
    }

    /// It prepares query without additional tracing information and warnings.
    /// Returns the prepared query.
    pub async fn prepare<Q: ToString + Sync + Send>(&self, query: Q) -> error::Result<PreparedQuery>
    where
        Self: Sync,
    {
        self.prepare_tw(query, false, false).await
    }

    /// Executes batch query with optional tracing and warnings.
    pub async fn batch_with_params_tw(
        &self,
        batch: QueryBatch,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let flags = prepare_flags(with_tracing, with_warnings);
        let is_idempotent = batch.is_idempotent;

        let query_frame = Frame::new_req_batch(batch, flags);

        send_frame(self, query_frame, is_idempotent).await
    }

    /// Executes batch query.
    pub async fn batch_with_params(&self, batch: QueryBatch) -> error::Result<Frame> {
        self.batch_with_params_tw(batch, false, false).await
    }

    /// Executes a query with parameters and ability to trace it and see warnings.
    pub async fn query_with_params_tw<Q: ToString + Send>(
        &self,
        query: Q,
        query_params: QueryParams,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let is_idempotent = query_params.is_idempotent;
        let query = Query {
            query: query.to_string(),
            params: query_params,
        };

        let flags = prepare_flags(with_tracing, with_warnings);

        let query_frame = Frame::new_query(query, flags);

        send_frame(self, query_frame, is_idempotent).await
    }

    /// Executes a query.
    pub async fn query<Q: ToString + Send>(&self, query: Q) -> error::Result<Frame> {
        self.query_tw(query, false, false).await
    }

    /// Executes a query with ability to trace it and see warnings.
    pub async fn query_tw<Q: ToString + Send>(
        &self,
        query: Q,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let query_params = QueryParamsBuilder::new().finalize();
        self.query_with_params_tw(query, query_params, with_tracing, with_warnings)
            .await
    }

    /// Executes a query with bounded values (either with or without names).
    pub async fn query_with_values<Q: ToString + Send, V: Into<QueryValues> + Send>(
        &self,
        query: Q,
        values: V,
    ) -> error::Result<Frame> {
        self.query_with_values_tw(query, values, false, false).await
    }

    /// Executes a query with bounded values (either with or without names)
    /// and ability to see warnings, trace a request and default parameters.
    pub async fn query_with_values_tw<Q: ToString + Send, V: Into<QueryValues> + Send>(
        &self,
        query: Q,
        values: V,
        with_tracing: bool,
        with_warnings: bool,
    ) -> error::Result<Frame> {
        let query_params_builder = QueryParamsBuilder::new();
        let query_params = query_params_builder.values(values.into()).finalize();
        self.query_with_params_tw(query, query_params, with_tracing, with_warnings)
            .await
    }

    /// Executes a query with query params without warnings and tracing.
    pub async fn query_with_params<Q: ToString + Send>(
        &self,
        query: Q,
        query_params: QueryParams,
    ) -> error::Result<Frame> {
        self.query_with_params_tw(query, query_params, false, false)
            .await
    }

    /// Returns connection from a load balancer.
    pub async fn load_balanced_connection(&self) -> Option<error::Result<Arc<T>>> {
        // when using a load balancer with > 1 node, don't use reconnection policy for a given node,
        // but jump to the next one

        let connection_manager = {
            if self.load_balancing.size() < 2 {
                self.load_balancing.next()
            } else {
                None
            }
        };

        if let Some(connection_manager) = connection_manager {
            let connection = connection_manager
                .connection(self.reconnection_policy.deref())
                .await;

            return match connection {
                Ok(connection) => Some(Ok(connection)),
                Err(error) => Some(Err(error)),
            };
        }

        loop {
            let connection_manager = self.load_balancing.next()?;
            let connection = connection_manager
                .connection(&NEVER_RECONNECTION_POLICY)
                .await;
            if let Ok(connection) = connection {
                return Some(Ok(connection));
            }
        }
    }

    /// Returns connection to the desired node.
    pub async fn node_connection(&self, node: &SocketAddr) -> Option<error::Result<Arc<T>>> {
        let connection_manager = self.load_balancing.find(|cm| cm.addr() == *node)?;

        Some(
            connection_manager
                .connection(self.reconnection_policy.deref())
                .await,
        )
    }

    fn new(
        load_balancing: LB,
        compression: Compression,
        transport_buffer_size: usize,
        tcp_nodelay: bool,
        retry_policy: Box<dyn RetryPolicy + Send + Sync>,
        reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    ) -> Self {
        Session {
            load_balancing,
            compression,
            transport_buffer_size,
            tcp_nodelay,
            retry_policy,
            reconnection_policy,
            _transport: Default::default(),
            _connection_manager: Default::default(),
        }
    }
}

impl<
        T: CdrsTransport + 'static,
        CM: ConnectionManager<T>,
        LB: LoadBalancingStrategy<CM> + Send + Sync,
    > GetRetryPolicy for Session<T, CM, LB>
{
    fn retry_policy(&self) -> &dyn RetryPolicy {
        self.retry_policy.as_ref()
    }
}

/// Workaround for <https://github.com/rust-lang/rust/issues/63033>
#[repr(transparent)]
pub struct RetryPolicyWrapper(pub Box<dyn RetryPolicy + Send + Sync>);

#[repr(transparent)]
pub struct ReconnectionPolicyWrapper(pub Box<dyn ReconnectionPolicy + Send + Sync>);

/// This function uses a user-supplied connection configuration to initialize all the
/// connections in the session. It can be used to supply your own transport and load
/// balancing mechanisms in order to support unusual node discovery mechanisms
/// or configuration needs.
///
/// The config object supplied differs from the ClusterTcpConfig and ClusterRustlsConfig
/// objects in that it is not expected to include an address. Instead the same configuration
/// will be applied to all connections across the cluster.
pub async fn connect_generic_static<T, C, A, CM, LB>(
    config: &C,
    initial_nodes: &[A],
    mut load_balancing: LB,
    compression: Compression,
    retry_policy: RetryPolicyWrapper,
    reconnection_policy: ReconnectionPolicyWrapper,
) -> error::Result<Session<T, CM, LB>>
where
    A: Clone,
    T: CdrsTransport + 'static,
    CM: ConnectionManager<T>,
    C: GenericClusterConfig<T, CM, Address = A>,
    LB: LoadBalancingStrategy<CM> + Sized + Send + Sync,
{
    let mut nodes = Vec::with_capacity(initial_nodes.len());

    for node in initial_nodes {
        let connection_manager = config.create_manager(node.clone()).await?;
        nodes.push(Arc::new(connection_manager));
    }

    load_balancing.init(nodes);

    Ok(Session {
        load_balancing,
        compression,
        transport_buffer_size: DEFAULT_TRANSPORT_BUFFER_SIZE,
        tcp_nodelay: true,
        retry_policy: retry_policy.0,
        reconnection_policy: reconnection_policy.0,
        _transport: Default::default(),
        _connection_manager: Default::default(),
    })
}

/// Creates new session that will perform queries without any compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new<LB>(
    node_configs: &ClusterTcpConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportTcp, TcpConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync,
{
    Ok(TcpSessionBuilder::new(load_balancing, node_configs.clone())
        .with_retry_policy(retry_policy)
        .with_reconnection_policy(reconnection_policy)
        .build())
}

/// Creates new session that will perform queries with Snappy compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new_snappy<LB>(
    node_configs: &ClusterTcpConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportTcp, TcpConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync,
{
    Ok(TcpSessionBuilder::new(load_balancing, node_configs.clone())
        .with_compression(Compression::Snappy)
        .with_retry_policy(retry_policy)
        .with_reconnection_policy(reconnection_policy)
        .build())
}

/// Creates new session that will perform queries with LZ4 compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new_lz4<LB>(
    node_configs: &ClusterTcpConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportTcp, TcpConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync,
{
    Ok(TcpSessionBuilder::new(load_balancing, node_configs.clone())
        .with_compression(Compression::Lz4)
        .with_retry_policy(retry_policy)
        .with_reconnection_policy(reconnection_policy)
        .build())
}

/// Creates new TLS session that will perform queries without any compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[cfg(feature = "rust-tls")]
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new_tls<LB>(
    node_configs: &ClusterRustlsConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportRustls, RustlsConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync,
{
    Ok(
        RustlsSessionBuilder::new(load_balancing, node_configs.clone())
            .with_retry_policy(retry_policy)
            .with_reconnection_policy(reconnection_policy)
            .build(),
    )
}

/// Creates new TLS session that will perform queries with Snappy compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[cfg(feature = "rust-tls")]
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new_snappy_tls<LB>(
    node_configs: &ClusterRustlsConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportRustls, RustlsConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync,
{
    Ok(
        RustlsSessionBuilder::new(load_balancing, node_configs.clone())
            .with_compression(Compression::Snappy)
            .with_retry_policy(retry_policy)
            .with_reconnection_policy(reconnection_policy)
            .build(),
    )
}

/// Creates new TLS session that will perform queries with LZ4 compression. `Compression` type
/// can be changed at any time.
/// As a parameter it takes:
/// * cluster config
/// * load balancing strategy (cannot be changed during `Session` life time).
#[cfg(feature = "rust-tls")]
#[deprecated(note = "Use SessionBuilder instead.")]
pub async fn new_lz4_tls<LB>(
    node_configs: &ClusterRustlsConfig,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
) -> error::Result<Session<TransportRustls, RustlsConnectionManager, LB>>
where
    LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync,
{
    Ok(
        RustlsSessionBuilder::new(load_balancing, node_configs.clone())
            .with_compression(Compression::Lz4)
            .with_retry_policy(retry_policy)
            .with_reconnection_policy(reconnection_policy)
            .build(),
    )
}

impl<
        T: CdrsTransport + 'static,
        CM: ConnectionManager<T>,
        LB: LoadBalancingStrategy<CM> + Send + Sync,
    > Session<T, CM, LB>
{
    /// Returns new event listener.
    pub async fn listen(
        &self,
        node: SocketAddr,
        authenticator: Arc<dyn SaslAuthenticatorProvider + Send + Sync>,
        events: Vec<SimpleServerEvent>,
    ) -> error::Result<(Listener, EventStream)> {
        let keyspace_holder = Arc::new(KeyspaceHolder::default());
        let config = NodeTcpConfigBuilder::new()
            .with_node_address(node.into())
            .with_authenticator_provider(authenticator)
            .build()
            .await?;
        let (event_sender, event_receiver) = channel(256);
        let connection_manager = TcpConnectionManager::new(
            config
                .get(0)
                .ok_or_else(|| error::Error::General("Empty node list!".into()))?
                .clone(),
            keyspace_holder,
            self.compression,
            self.transport_buffer_size,
            self.tcp_nodelay,
            Some(event_sender),
        );
        let transport = connection_manager
            .connection(&NeverReconnectionPolicy)
            .await?;

        let query_frame = Frame::new_req_register(events);
        transport.write_frame(&query_frame).await?;

        let (sender, receiver) = std_channel();
        Ok((
            new_listener(sender, event_receiver),
            EventStream::new(receiver),
        ))
    }

    #[cfg(feature = "rust-tls")]
    pub async fn listen_tls(
        &self,
        node: net::SocketAddr,
        authenticator: Arc<dyn SaslAuthenticatorProvider + Send + Sync>,
        events: Vec<SimpleServerEvent>,
        dns_name: webpki::DNSName,
        config: Arc<rustls::ClientConfig>,
    ) -> error::Result<(Listener, EventStream)> {
        let keyspace_holder = Arc::new(KeyspaceHolder::default());
        let config = NodeRustlsConfigBuilder::new(dns_name, config)
            .with_node_address(node.into())
            .with_authenticator_provider(authenticator)
            .build()
            .await?;
        let (event_sender, event_receiver) = channel(256);
        let connection_manager = RustlsConnectionManager::new(
            config
                .get(0)
                .ok_or_else(|| error::Error::General("Empty node list!".into()))?
                .clone(),
            keyspace_holder,
            self.compression,
            self.transport_buffer_size,
            self.tcp_nodelay,
            Some(event_sender),
        );
        let transport = connection_manager
            .connection(&NeverReconnectionPolicy)
            .await?;

        let query_frame = Frame::new_req_register(events);
        transport.write_frame(&query_frame).await?;

        let (sender, receiver) = std_channel();
        Ok((
            new_listener(sender, event_receiver),
            EventStream::new(receiver),
        ))
    }

    pub async fn listen_non_blocking(
        &self,
        node: SocketAddr,
        authenticator: Arc<dyn SaslAuthenticatorProvider + Send + Sync>,
        events: Vec<SimpleServerEvent>,
    ) -> error::Result<(Listener, EventStreamNonBlocking)> {
        self.listen(node, authenticator, events).await.map(|l| {
            let (listener, stream) = l;
            (listener, stream.into())
        })
    }

    #[cfg(feature = "rust-tls")]
    pub async fn listen_tls_blocking(
        &self,
        node: net::SocketAddr,
        authenticator: Arc<dyn SaslAuthenticatorProvider + Send + Sync>,
        events: Vec<SimpleServerEvent>,
        dns_name: webpki::DNSName,
        config: Arc<rustls::ClientConfig>,
    ) -> error::Result<(Listener, EventStreamNonBlocking)> {
        self.listen_tls(node, authenticator, events, dns_name, config)
            .await
            .map(|l| {
                let (listener, stream) = l;
                (listener, stream.into())
            })
    }
}

struct SessionConfig<CM, LB: LoadBalancingStrategy<CM> + Send + Sync> {
    compression: Compression,
    transport_buffer_size: usize,
    tcp_nodelay: bool,
    load_balancing: LB,
    retry_policy: Box<dyn RetryPolicy + Send + Sync>,
    reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    _connection_manager: PhantomData<CM>,
}

impl<CM, LB: LoadBalancingStrategy<CM> + Send + Sync> SessionConfig<CM, LB> {
    fn new(
        compression: Compression,
        transport_buffer_size: usize,
        tcp_nodelay: bool,
        load_balancing: LB,
        retry_policy: Box<dyn RetryPolicy + Send + Sync>,
        reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    ) -> Self {
        SessionConfig {
            compression,
            transport_buffer_size,
            tcp_nodelay,
            load_balancing,
            retry_policy,
            reconnection_policy,
            _connection_manager: Default::default(),
        }
    }
}

/// Builder for easy `Session` creation. Requires static `LoadBalancingStrategy`, but otherwise, other
/// configuration parameters can be dynamically set. Use concrete implementers to create specific
/// sessions.
pub trait SessionBuilder<
    T: CdrsTransport + Send + Sync + 'static,
    CM: ConnectionManager<T>,
    LB: LoadBalancingStrategy<CM> + Send + Sync,
>
{
    /// Sets new compression.
    fn with_compression(self, compression: Compression) -> Self;

    /// Set new retry policy.
    fn with_retry_policy(self, retry_policy: Box<dyn RetryPolicy + Send + Sync>) -> Self;

    /// Set new reconnection policy.
    fn with_reconnection_policy(
        self,
        reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    ) -> Self;

    /// Sets new transport buffer size. High values are recommended with large amounts of in flight
    /// queries.
    fn with_transport_buffer_size(self, transport_buffer_size: usize) -> Self;

    /// Sets NODELAY for given session connections.
    fn with_tcp_nodelay(self, tcp_nodelay: bool) -> Self;

    /// Builds the resulting session.
    fn build(self) -> Session<T, CM, LB>;
}

/// Builder for non-TLS sessions.
pub struct TcpSessionBuilder<LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync> {
    config: SessionConfig<TcpConnectionManager, LB>,
    node_configs: ClusterTcpConfig,
}

impl<LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync> TcpSessionBuilder<LB> {
    /// Creates a new builder with default session configuration.
    pub fn new(load_balancing: LB, node_configs: ClusterTcpConfig) -> Self {
        TcpSessionBuilder {
            config: SessionConfig::new(
                Compression::None,
                DEFAULT_TRANSPORT_BUFFER_SIZE,
                true,
                load_balancing,
                Box::new(DefaultRetryPolicy::default()),
                Box::new(ExponentialReconnectionPolicy::default()),
            ),
            node_configs,
        }
    }
}

impl<LB: LoadBalancingStrategy<TcpConnectionManager> + Send + Sync>
    SessionBuilder<TransportTcp, TcpConnectionManager, LB> for TcpSessionBuilder<LB>
{
    fn with_compression(mut self, compression: Compression) -> Self {
        self.config.compression = compression;
        self
    }

    fn with_retry_policy(mut self, retry_policy: Box<dyn RetryPolicy + Send + Sync>) -> Self {
        self.config.retry_policy = retry_policy;
        self
    }

    fn with_reconnection_policy(
        mut self,
        reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    ) -> Self {
        self.config.reconnection_policy = reconnection_policy;
        self
    }

    fn with_transport_buffer_size(mut self, transport_buffer_size: usize) -> Self {
        self.config.transport_buffer_size = transport_buffer_size;
        self
    }

    fn with_tcp_nodelay(mut self, tcp_nodelay: bool) -> Self {
        self.config.tcp_nodelay = tcp_nodelay;
        self
    }

    fn build(mut self) -> Session<TransportTcp, TcpConnectionManager, LB> {
        let keyspace_holder = Arc::new(KeyspaceHolder::default());
        let mut nodes = Vec::with_capacity(self.node_configs.0.len());

        for node_config in self.node_configs.0 {
            let connection_manager = TcpConnectionManager::new(
                node_config,
                keyspace_holder.clone(),
                self.config.compression,
                self.config.transport_buffer_size,
                self.config.tcp_nodelay,
                None,
            );
            nodes.push(Arc::new(connection_manager));
        }

        self.config.load_balancing.init(nodes);

        Session::new(
            self.config.load_balancing,
            self.config.compression,
            self.config.transport_buffer_size,
            self.config.tcp_nodelay,
            self.config.retry_policy,
            self.config.reconnection_policy,
        )
    }
}

#[cfg(feature = "rust-tls")]
/// Builder for TLS sessions.
pub struct RustlsSessionBuilder<LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync> {
    config: SessionConfig<RustlsConnectionManager, LB>,
    node_configs: ClusterRustlsConfig,
}

#[cfg(feature = "rust-tls")]
impl<LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync> RustlsSessionBuilder<LB> {
    /// Creates a new builder with default session configuration.
    pub fn new(load_balancing: LB, node_configs: ClusterRustlsConfig) -> Self {
        RustlsSessionBuilder {
            config: SessionConfig::new(
                Compression::None,
                DEFAULT_TRANSPORT_BUFFER_SIZE,
                true,
                load_balancing,
                Box::new(DefaultRetryPolicy::default()),
                Box::new(ExponentialReconnectionPolicy::default()),
            ),
            node_configs,
        }
    }
}

#[cfg(feature = "rust-tls")]
impl<LB: LoadBalancingStrategy<RustlsConnectionManager> + Send + Sync>
    SessionBuilder<TransportRustls, RustlsConnectionManager, LB> for RustlsSessionBuilder<LB>
{
    fn with_compression(mut self, compression: Compression) -> Self {
        self.config.compression = compression;
        self
    }

    fn with_retry_policy(mut self, retry_policy: Box<dyn RetryPolicy + Send + Sync>) -> Self {
        self.config.retry_policy = retry_policy;
        self
    }

    fn with_reconnection_policy(
        mut self,
        reconnection_policy: Box<dyn ReconnectionPolicy + Send + Sync>,
    ) -> Self {
        self.config.reconnection_policy = reconnection_policy;
        self
    }

    fn with_transport_buffer_size(mut self, transport_buffer_size: usize) -> Self {
        self.config.transport_buffer_size = transport_buffer_size;
        self
    }

    fn with_tcp_nodelay(mut self, tcp_nodelay: bool) -> Self {
        self.config.tcp_nodelay = tcp_nodelay;
        self
    }

    fn build(mut self) -> Session<TransportRustls, RustlsConnectionManager, LB> {
        let keyspace_holder = Arc::new(KeyspaceHolder::default());
        let mut nodes = Vec::with_capacity(self.node_configs.0.len());

        for node_config in self.node_configs.0 {
            let connection_manager = RustlsConnectionManager::new(
                node_config,
                keyspace_holder.clone(),
                self.config.compression,
                self.config.transport_buffer_size,
                self.config.tcp_nodelay,
                None,
            );
            nodes.push(Arc::new(connection_manager));
        }

        self.config.load_balancing.init(nodes);

        Session::new(
            self.config.load_balancing,
            self.config.compression,
            self.config.transport_buffer_size,
            self.config.tcp_nodelay,
            self.config.retry_policy,
            self.config.reconnection_policy,
        )
    }
}
