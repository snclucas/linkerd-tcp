use super::{ConfigError, admin, resolver, router, server};
use super::balancer::BalancerFactory;
use super::connector::ConnectorFactoryConfig;
use super::resolver::NamerdConfig;
use futures::{Future, Stream};
use futures::sync::oneshot;
use hyper::server::Http;
use serde_json;
use serde_yaml;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::net;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tacho;
use tokio_core::net::TcpListener;
use tokio_core::reactor::{Core, Handle};
use tokio_timer::Timer;

const DEFAULT_ADMIN_PORT: u16 = 9989;
const DEFAULT_BUFFER_SIZE_BYTES: usize = 16 * 1024;
const DEFAULT_GRACE_SECS: u64 = 10;
const DEFAULT_METRICS_INTERVAL_SECS: u64 = 60;
//TODO const DEFAULT_MINIMUM_CONNECTIONS: usize = 1;
//TODO const DEFAULT_MAXIMUM_WAITERS: usize = 128;

/// Signals a receiver to shutdown by the provided deadline.
pub type Closer = oneshot::Sender<Instant>;

/// Signals that the receiver should release its resources by the provided deadline.
pub type Closed = oneshot::Receiver<Instant>;

/// Creates a thread-safe shutdown latch.
pub fn closer() -> (Closer, Closed) {
    oneshot::channel()
}

/// Holds the configuration for a linkerd-tcp instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppConfig {
    /// Configures the processes's admin server.
    pub admin: Option<AdminConfig>,

    /// Configures one or more routers.
    pub routers: Vec<RouterConfig>,

    /// Configures the shared buffer used for transferring data.
    pub buffer_size_bytes: Option<usize>,
}

impl ::std::str::FromStr for AppConfig {
    type Err = ConfigError;

    /// Parses a JSON- or YAML-formatted configuration file.
    fn from_str(txt: &str) -> Result<AppConfig, ConfigError> {
        let txt = txt.trim_left();
        if txt.starts_with('{') {
            serde_json::from_str(txt).map_err(|e| format!("json error: {}", e).into())
        } else {
            serde_yaml::from_str(txt).map_err(|e| format!("yaml error: {}", e).into())
        }
    }
}

impl AppConfig {
    /// Build an App from a configuration.
    pub fn into_app(mut self) -> Result<App, ConfigError> {
        // Create a shared transfer buffer to be used for all stream proxying.
        let buf = {
            let sz = self.buffer_size_bytes.unwrap_or(DEFAULT_BUFFER_SIZE_BYTES);
            Rc::new(RefCell::new(vec![0 as u8; sz]))
        };

        let (metrics, reporter) = tacho::new();

        // Load all router configurations.
        //
        // Separate resolver tasks are created to be executed in the admin thread's
        // reactor so that service discovery lookups are performed out of the serving
        // thread.
        let mut routers = VecDeque::with_capacity(self.routers.len());
        let mut resolvers = VecDeque::with_capacity(self.routers.len());
        for config in self.routers.drain(..) {
            let mut r = config.into_router(buf.clone(), &metrics)?;
            let e = r.resolver_executor
                .take()
                .expect("router missing resolver executor");
            routers.push_back(r);
            resolvers.push_back(e);
        }

        // Read the admin server configuration and bundle it an AdminRunner.
        let admin = {
            let addr = {
                let ip = self.admin
                    .as_ref()
                    .and_then(|a| a.ip)
                    .or_else(|| {
                                 "127.0.0.1"
                                     .parse::<net::Ipv4Addr>()
                                     .ok()
                                     .map(net::IpAddr::V4)
                             })
                    .unwrap();
                let port = self.admin
                    .as_ref()
                    .and_then(|a| a.port)
                    .unwrap_or(DEFAULT_ADMIN_PORT);
                net::SocketAddr::new(ip, port)
            };
            let grace = Duration::from_secs(self.admin
                                                .as_ref()
                                                .and_then(|admin| admin.grace_secs)
                                                .unwrap_or(DEFAULT_GRACE_SECS));
            let metrics_interval =
                Duration::from_secs(self.admin
                                        .as_ref()
                                        .and_then(|admin| admin.metrics_interval_secs)
                                        .unwrap_or(DEFAULT_METRICS_INTERVAL_SECS));
            AdminRunner {
                addr,
                reporter,
                resolvers,
                grace,
                metrics_interval,
            }
        };

        Ok(App {
               routers: routers,
               admin: admin,
           })
    }
}

/// Holds configuraed tasks to be spawned.
pub struct App {
    /// Executes configured routers.
    pub routers: VecDeque<RouterSpawner>,
    /// Executes the admin server.
    pub admin: AdminRunner,
}

/// Holds the configuration for a single stream router.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouterConfig {
    /// A descriptive name for this router. For stats reporting.
    pub label: String,

    /// The configuration for one or more servers.
    pub servers: Vec<server::ServerConfig>,

    /// Determines how outbound connections are initiated.
    ///
    /// By default, connections are clear TCP.
    pub client: Option<ConnectorFactoryConfig>,

    pub interpreter: NamerdConfig,

    //pub minimum_connections: Option<usize>,
    // TODO pub maximum_waiters: Option<usize>,
}

impl RouterConfig {
    /// Consumes and validates this configuration to produce a router initializer.
    fn into_router(mut self,
                   buf: Rc<RefCell<Vec<u8>>>,
                   metrics: &tacho::Scope)
                   -> Result<RouterSpawner, ConfigError> {

        // Each router has its own resolver/executor pair. The resolver is used by the
        // router. The resolver executor is used to drive exececution in another thread.
        let (resolver, resolver_exec) = {
            let namerd = self.interpreter.into_namerd(&metrics)?;
            resolver::new(namerd)
        };

        let balancer = {
            //let min_conns = self.minimum_connections
            //    .unwrap_or(DEFAULT_MINIMUM_CONNECTIONS);
            let client = self.client.unwrap_or_default().mk_connector_factory()?;
            BalancerFactory::new(/*min_conns,*/
                                 client)
        };
        let router = router::new(resolver, balancer);

        let mut servers = VecDeque::with_capacity(self.servers.len());
        for config in self.servers.drain(..) {
            let server = config.mk_server(router.clone(), buf.clone())?;
            servers.push_back(server);
        }

        Ok(RouterSpawner {
               servers: servers,
               resolver_executor: Some(resolver_exec),
           })
    }
}

pub struct RouterSpawner {
    servers: VecDeque<server::Unbound>,
    resolver_executor: Option<resolver::Executor>,
}

impl RouterSpawner {
    pub fn spawn(mut self, reactor: &Handle, timer: &Timer) -> Result<(), ConfigError> {
        while let Some(unbound) = self.servers.pop_front() {
            info!("routing on {} to {}",
                  unbound.listen_addr(),
                  unbound.dst_name());
            let bound = unbound.bind(reactor, timer).expect("failed to bind");
            reactor.spawn(bound.map_err(|_| {}));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdminConfig {
    pub port: Option<u16>,
    pub ip: Option<net::IpAddr>,
    pub metrics_interval_secs: Option<u64>,
    pub grace_secs: Option<u64>,
}

pub struct AdminRunner {
    addr: net::SocketAddr,
    reporter: tacho::Reporter,
    resolvers: VecDeque<resolver::Executor>,
    grace: Duration,
    metrics_interval: Duration,
}

impl AdminRunner {
    pub fn run(self, closer: Closer, reactor: &mut Core, timer: &Timer) -> Result<(), ConfigError> {
        let AdminRunner {
            addr,
            grace,
            metrics_interval,
            mut reporter,
            mut resolvers,
        } = self;

        let handle = reactor.handle();
        {
            while let Some(resolver) = resolvers.pop_front() {
                handle.spawn(resolver.execute(&handle, timer));
            }
        }

        let prometheus = Rc::new(RefCell::new(String::new()));
        let reporting = {
            let prometheus = prometheus.clone();
            timer
                .interval(metrics_interval)
                .map_err(|_| {})
                .for_each(move |_| {
                              let report = reporter.take();
                              let mut export = prometheus.borrow_mut();
                              *export = tacho::prometheus::format(&report);
                              Ok(())
                          })
        };
        handle.spawn(reporting);

        let serving = {
            let listener = {
                println!("Listening on http://{}.", addr);
                TcpListener::bind(&addr, &handle).expect("unable to listen")
            };

            let server =
                admin::Admin::new(prometheus, closer, grace, handle.clone(), timer.clone());
            let http = Http::new();
            listener
                .incoming()
                .for_each(move |(tcp, src)| {
                              http.bind_connection(&handle, tcp, src, server.clone());
                              Ok(())
                          })
        };
        reactor.run(serving).unwrap();

        Ok(())
    }
}