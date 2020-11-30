use super::collector::{self, MetricCollector as _};
use crate::{
    buffers::Acker,
    config::{DataType, GenerateConfig, Resource, SinkConfig, SinkContext, SinkDescription},
    event::metric::{Metric, MetricKind},
    internal_events::PrometheusServerRequestComplete,
    sinks::{
        util::{statistic::validate_quantiles, MetricEntry, StreamSink},
        Healthcheck, VectorSink,
    },
    stream::tripwire_handler,
    Event,
};
use async_trait::async_trait;
use futures::{future, stream::BoxStream, FutureExt, StreamExt, TryFutureExt};
use hyper::{
    header::HeaderValue,
    service::{make_service_fn, service_fn},
    Body, Method, Request, Response, Server, StatusCode,
};
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::{
    collections::HashSet,
    convert::Infallible,
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use stream_cancel::{Trigger, Tripwire};

const MIN_FLUSH_PERIOD_SECS: u64 = 1;

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Flush period for sets must be greater or equal to {} secs", min))]
    FlushPeriodTooShort { min: u64 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrometheusExporterConfig {
    pub default_namespace: Option<String>,
    #[serde(default = "default_address")]
    pub address: SocketAddr,
    #[serde(default = "super::default_histogram_buckets")]
    pub buckets: Vec<f64>,
    #[serde(default = "super::default_summary_quantiles")]
    pub quantiles: Vec<f64>,
    #[serde(default = "default_flush_period_secs")]
    pub flush_period_secs: u64,
}

impl std::default::Default for PrometheusExporterConfig {
    fn default() -> Self {
        Self {
            default_namespace: None,
            address: default_address(),
            buckets: super::default_histogram_buckets(),
            quantiles: super::default_summary_quantiles(),
            flush_period_secs: default_flush_period_secs(),
        }
    }
}

fn default_address() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9598)
}

fn default_flush_period_secs() -> u64 {
    60
}

inventory::submit! {
    SinkDescription::new::<PrometheusExporterConfig>("prometheus")
}

inventory::submit! {
    SinkDescription::new::<PrometheusExporterConfig>("prometheus_exporter")
}

impl GenerateConfig for PrometheusExporterConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(&Self::default()).unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "prometheus_exporter")]
impl SinkConfig for PrometheusExporterConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        if self.flush_period_secs < MIN_FLUSH_PERIOD_SECS {
            return Err(Box::new(BuildError::FlushPeriodTooShort {
                min: MIN_FLUSH_PERIOD_SECS,
            }));
        }

        validate_quantiles(&self.quantiles)?;

        let sink = PrometheusExporter::new(self.clone(), cx.acker());
        let healthcheck = future::ok(()).boxed();

        Ok((VectorSink::Stream(Box::new(sink)), healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Metric
    }

    fn sink_type(&self) -> &'static str {
        "prometheus_exporter"
    }

    fn resources(&self) -> Vec<Resource> {
        vec![self.address.into()]
    }
}

// Add a compatibility alias to avoid breaking existing configs
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PrometheusCompatConfig {
    #[serde(flatten)]
    config: PrometheusExporterConfig,
}

#[async_trait::async_trait]
#[typetag::serde(name = "prometheus")]
impl SinkConfig for PrometheusCompatConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        self.config.build(cx).await
    }

    fn input_type(&self) -> DataType {
        self.config.input_type()
    }

    fn sink_type(&self) -> &'static str {
        "prometheus"
    }

    fn resources(&self) -> Vec<Resource> {
        self.config.resources()
    }
}

struct PrometheusExporter {
    server_shutdown_trigger: Option<Trigger>,
    config: PrometheusExporterConfig,
    metrics: Arc<RwLock<IndexSet<ExpiringMetric>>>,
    acker: Acker,
}

#[derive(Debug)]
struct ExpiringMetric {
    inner: MetricEntry,
    next_flush: Instant,
}

impl ExpiringMetric {
    fn new(metric: Metric, next_flush: Instant) -> Self {
        Self {
            inner: MetricEntry::new(metric),
            next_flush,
        }
    }

    fn is_expired(&self, time: Instant) -> bool {
        self.next_flush < time
    }

    fn get_ref(&self) -> &Metric {
        self.inner.get_ref()
    }

    fn get_mut(&mut self) -> &mut Metric {
        self.inner.get_mut()
    }
}

impl Hash for ExpiringMetric {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state)
    }
}

impl PartialEq for ExpiringMetric {
    fn eq(&self, other: &Self) -> bool {
        self.inner.eq(&other.inner)
    }
}

impl Eq for ExpiringMetric {}

fn handle(
    req: Request<Body>,
    default_namespace: Option<&str>,
    buckets: &[f64],
    quantiles: &[f64],
    metrics: &IndexSet<ExpiringMetric>,
) -> Response<Body> {
    let mut response = Response::new(Body::empty());

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => {
            let mut s = collector::StringCollector::new();

            // output headers only once
            let mut processed_headers = HashSet::new();

            let now = Instant::now();
            for item in metrics {
                let metric = item.get_ref();
                if !processed_headers.contains(&metric.name) {
                    s.encode_header(default_namespace, metric);
                    processed_headers.insert(&metric.name);
                };

                s.encode_metric(
                    default_namespace,
                    &buckets,
                    quantiles,
                    item.is_expired(now),
                    metric,
                );
            }

            *response.body_mut() = s.result.into();

            response.headers_mut().insert(
                "Content-Type",
                HeaderValue::from_static("text/plain; version=0.0.4"),
            );
        }
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    }

    response
}

impl PrometheusExporter {
    fn new(config: PrometheusExporterConfig, acker: Acker) -> Self {
        Self {
            server_shutdown_trigger: None,
            config,
            metrics: Arc::new(RwLock::new(IndexSet::new())),
            acker,
        }
    }

    fn start_server_if_needed(&mut self) {
        if self.server_shutdown_trigger.is_some() {
            return;
        }

        let metrics = Arc::clone(&self.metrics);
        let default_namespace = self.config.default_namespace.clone();
        let buckets = self.config.buckets.clone();
        let quantiles = self.config.quantiles.clone();

        let new_service = make_service_fn(move |_| {
            let metrics = Arc::clone(&metrics);
            let default_namespace = default_namespace.clone();
            let buckets = buckets.clone();
            let quantiles = quantiles.clone();

            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let metrics = metrics.read().unwrap();

                    let response = info_span!(
                        "prometheus_server",
                        method = ?req.method(),
                        path = ?req.uri().path(),
                    )
                    .in_scope(|| {
                        handle(
                            req,
                            default_namespace.as_deref(),
                            &buckets,
                            &quantiles,
                            &metrics,
                        )
                    });

                    emit!(PrometheusServerRequestComplete {
                        status_code: response.status(),
                    });

                    future::ok::<_, Infallible>(response)
                }))
            }
        });

        let (trigger, tripwire) = Tripwire::new();

        let server = Server::bind(&self.config.address)
            .serve(new_service)
            .with_graceful_shutdown(tripwire.then(tripwire_handler))
            .map_err(|error| eprintln!("Server error: {}", error));

        tokio::spawn(server);
        self.server_shutdown_trigger = Some(trigger);
    }
}

#[async_trait]
impl StreamSink for PrometheusExporter {
    async fn run(&mut self, mut input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.start_server_if_needed();
        while let Some(event) = input.next().await {
            let item = event.into_metric();
            let mut metrics = self.metrics.write().unwrap();

            let now = Instant::now();
            let next_flush = now + Duration::from_secs(self.config.flush_period_secs);
            match item.kind {
                MetricKind::Incremental => {
                    let new = ExpiringMetric::new(item.to_absolute(), next_flush);
                    if let Some(mut entry) = metrics.take(&new) {
                        // sets need to be expired from time to time
                        // because otherwise they could grow infinitelly
                        if item.value.is_set() && entry.is_expired(now) {
                            entry.get_mut().reset();
                            entry.next_flush = next_flush;
                        }
                        entry.get_mut().add(&item);
                        metrics.insert(entry);
                    } else {
                        metrics.insert(new);
                    };
                }
                MetricKind::Absolute => {
                    let new = ExpiringMetric::new(item, next_flush);
                    metrics.replace(new);
                }
            };

            self.acker.ack(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<PrometheusExporterConfig>();
    }
}

#[cfg(all(test, feature = "prometheus-integration-tests"))]
mod integration_tests {
    use super::*;
    use crate::{
        event::{Metric, MetricValue},
        http::HttpClient,
        test_util::{random_string, trace_init},
    };
    use chrono::Utc;
    use serde_json::Value;
    use tokio::{sync::mpsc, time};

    const PROMETHEUS_ADDRESS: &str = "127.0.0.1:9101";

    #[tokio::test]
    async fn prometheus_metrics() {
        trace_init();

        prometheus_scrapes_metrics().await;
        time::delay_for(time::Duration::from_millis(500)).await;
        reset_on_flush_period().await;
    }

    async fn prometheus_scrapes_metrics() {
        let start = Utc::now().timestamp();

        let config = PrometheusExporterConfig {
            address: PROMETHEUS_ADDRESS.parse().unwrap(),
            ..Default::default()
        };
        let (sink, _) = config.build(SinkContext::new_test()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(sink.run(Box::pin(rx)));

        let (name, event) = create_metric_gauge(None, 123.4);
        tx.send(event).expect("Failed to send.");

        // Wait a bit for the prometheus server to scrape the metrics
        time::delay_for(time::Duration::from_secs(2)).await;

        // Now try to download them from prometheus
        let result = prometheus_query(&name).await;

        let data = &result["data"]["result"][0];
        assert_eq!(data["metric"]["__name__"], Value::String(name));
        assert_eq!(
            data["metric"]["instance"],
            Value::String(PROMETHEUS_ADDRESS.into())
        );
        assert_eq!(
            data["metric"]["some_tag"],
            Value::String("some_value".into())
        );
        assert!(data["value"][0].as_f64().unwrap() >= start as f64);
        assert_eq!(data["value"][1], Value::String("123.4".into()));
    }

    async fn reset_on_flush_period() {
        let config = PrometheusExporterConfig {
            address: PROMETHEUS_ADDRESS.parse().unwrap(),
            flush_period_secs: 3,
            ..Default::default()
        };
        let (sink, _) = config.build(SinkContext::new_test()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(sink.run(Box::pin(rx)));

        let (name1, event) = create_metric_set(None, vec!["0", "1", "2"]);
        tx.send(event).expect("Failed to send.");
        let (name2, event) = create_metric_set(None, vec!["3", "4", "5"]);
        tx.send(event).expect("Failed to send.");

        // Wait a bit for the prometheus server to scrape the metrics
        time::delay_for(time::Duration::from_secs(2)).await;

        // Now try to download them from prometheus
        let result = prometheus_query(&name1).await;
        assert_eq!(
            result["data"]["result"][0]["value"][1],
            Value::String("3".into())
        );
        let result = prometheus_query(&name2).await;
        assert_eq!(
            result["data"]["result"][0]["value"][1],
            Value::String("3".into())
        );

        // Wait a bit for expired metrics
        time::delay_for(time::Duration::from_secs(3)).await;

        let (name1, event) = create_metric_set(Some(name1), vec!["6", "7"]);
        tx.send(event).expect("Failed to send.");
        let (name2, event) = create_metric_set(Some(name2), vec!["8", "9"]);
        tx.send(event).expect("Failed to send.");

        // Wait a bit for the prometheus server to scrape the metrics
        time::delay_for(time::Duration::from_secs(2)).await;

        // Now try to download them from prometheus
        let result = prometheus_query(&name1).await;
        assert_eq!(
            result["data"]["result"][0]["value"][1],
            Value::String("2".into())
        );
        let result = prometheus_query(&name2).await;
        assert_eq!(
            result["data"]["result"][0]["value"][1],
            Value::String("2".into())
        );
    }

    async fn prometheus_query(query: &str) -> Value {
        let uri = format!("http://127.0.0.1:9090/api/v1/query?query={}", query)
            .parse::<http::Uri>()
            .expect("Invalid query URL");
        let request = Request::post(uri)
            .body(Body::empty())
            .expect("Error creating request.");
        let result = HttpClient::new(None)
            .unwrap()
            .send(request)
            .await
            .expect("Could not fetch query");
        let result = hyper::body::to_bytes(result.into_body())
            .await
            .expect("Error fetching body");
        let result = String::from_utf8_lossy(&result);
        serde_json::from_str(result.as_ref()).expect("Invalid JSON from prometheus")
    }

    fn create_metric_gauge(name: Option<String>, value: f64) -> (String, Event) {
        create_metric(name, MetricValue::Gauge { value })
    }

    fn create_metric_set(name: Option<String>, values: Vec<&'static str>) -> (String, Event) {
        create_metric(
            name,
            MetricValue::Set {
                values: values.into_iter().map(Into::into).collect(),
            },
        )
    }

    fn create_metric(name: Option<String>, value: MetricValue) -> (String, Event) {
        let name = name.unwrap_or_else(|| format!("vector_set_{}", random_string(16)));
        let event = Metric {
            name: name.clone(),
            namespace: None,
            timestamp: None,
            tags: Some(
                vec![("some_tag".to_owned(), "some_value".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            kind: MetricKind::Incremental,
            value,
        }
        .into();
        (name, event)
    }
}
