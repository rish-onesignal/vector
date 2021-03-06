use crate::{
    buffers::Acker,
    config::{DataType, GenerateConfig, SinkConfig, SinkContext, SinkDescription},
    event::metric::{Metric, MetricKind, MetricValue, StatisticKind},
    sinks::util::{
        encode_namespace,
        statistic::{validate_quantiles, DistributionStatistic},
        MetricEntry, StreamSink,
    },
    Event,
};
use async_trait::async_trait;
use chrono::Utc;
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
    collections::{BTreeMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, RwLock},
};
use stream_cancel::{Trigger, Tripwire};

const MIN_FLUSH_PERIOD_SECS: u64 = 1;

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Flush period for sets must be greater or equal to {} secs", min))]
    FlushPeriodTooShort { min: u64 },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PrometheusSinkConfig {
    pub namespace: Option<String>,
    #[serde(default = "default_address")]
    pub address: SocketAddr,
    #[serde(default = "default_histogram_buckets")]
    pub buckets: Vec<f64>,
    #[serde(default = "default_summary_quantiles")]
    pub quantiles: Vec<f64>,
    #[serde(default = "default_flush_period_secs")]
    pub flush_period_secs: u64,
}

pub fn default_histogram_buckets() -> Vec<f64> {
    vec![
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ]
}

pub fn default_summary_quantiles() -> Vec<f64> {
    vec![0.5, 0.75, 0.9, 0.95, 0.99]
}

pub fn default_address() -> SocketAddr {
    use std::net::{IpAddr, Ipv4Addr};

    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9598)
}

pub fn default_flush_period_secs() -> u64 {
    60
}

inventory::submit! {
    SinkDescription::new::<PrometheusSinkConfig>("prometheus")
}

impl GenerateConfig for PrometheusSinkConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(&Self {
            namespace: None,
            address: default_address(),
            buckets: default_histogram_buckets(),
            quantiles: default_summary_quantiles(),
            flush_period_secs: default_flush_period_secs(),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "prometheus")]
impl SinkConfig for PrometheusSinkConfig {
    async fn build(
        &self,
        cx: SinkContext,
    ) -> crate::Result<(super::VectorSink, super::Healthcheck)> {
        if self.flush_period_secs < MIN_FLUSH_PERIOD_SECS {
            return Err(Box::new(BuildError::FlushPeriodTooShort {
                min: MIN_FLUSH_PERIOD_SECS,
            }));
        }

        validate_quantiles(&self.quantiles)?;

        let sink = PrometheusSink::new(self.clone(), cx.acker());
        let healthcheck = future::ok(()).boxed();

        Ok((super::VectorSink::Stream(Box::new(sink)), healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Metric
    }

    fn sink_type(&self) -> &'static str {
        "prometheus"
    }
}

struct PrometheusSink {
    server_shutdown_trigger: Option<Trigger>,
    config: PrometheusSinkConfig,
    metrics: Arc<RwLock<IndexSet<MetricEntry>>>,
    last_flush_timestamp: Arc<RwLock<i64>>,
    acker: Acker,
}

fn encode_tags(tags: &Option<BTreeMap<String, String>>) -> String {
    if let Some(tags) = tags {
        let mut parts: Vec<_> = tags
            .iter()
            .map(|(name, value)| format!("{}=\"{}\"", name, value))
            .collect();

        parts.sort();
        format!("{{{}}}", parts.join(","))
    } else {
        String::from("")
    }
}

fn encode_tags_with_extra(
    tags: &Option<BTreeMap<String, String>>,
    tag: String,
    value: String,
) -> String {
    let mut parts: Vec<_> = if let Some(tags) = tags {
        tags.iter()
            .chain(vec![(&tag, &value)])
            .map(|(name, value)| format!("{}=\"{}\"", name, value))
            .collect()
    } else {
        vec![format!("{}=\"{}\"", tag, value)]
    };

    parts.sort();
    format!("{{{}}}", parts.join(","))
}

fn encode_metric_header(namespace: Option<&str>, metric: &Metric) -> String {
    let mut s = String::new();
    let name = &metric.name;
    let fullname = encode_namespace(namespace, '_', name);

    let r#type = match &metric.value {
        MetricValue::Counter { .. } => "counter",
        MetricValue::Gauge { .. } => "gauge",
        MetricValue::Distribution {
            statistic: StatisticKind::Histogram,
            ..
        } => "histogram",
        MetricValue::Distribution {
            statistic: StatisticKind::Summary,
            ..
        } => "summary",
        MetricValue::Set { .. } => "gauge",
        MetricValue::AggregatedHistogram { .. } => "histogram",
        MetricValue::AggregatedSummary { .. } => "summary",
    };

    s.push_str(&format!("# HELP {} {}\n", fullname, name));
    s.push_str(&format!("# TYPE {} {}\n", fullname, r#type));
    s
}

fn encode_metric_datum(
    namespace: Option<&str>,
    buckets: &[f64],
    quantiles: &[f64],
    expired: bool,
    metric: &Metric,
) -> String {
    let mut s = String::new();
    let fullname = encode_namespace(namespace, '_', &metric.name);

    if metric.kind.is_absolute() {
        let tags = &metric.tags;

        match &metric.value {
            MetricValue::Counter { value } => {
                s.push_str(&format!("{}{} {}\n", fullname, encode_tags(tags), value));
            }
            MetricValue::Gauge { value } => {
                s.push_str(&format!("{}{} {}\n", fullname, encode_tags(tags), value));
            }
            MetricValue::Set { values } => {
                // sets could expire
                let value = if expired { 0 } else { values.len() };
                s.push_str(&format!("{}{} {}\n", fullname, encode_tags(tags), value));
            }
            MetricValue::Distribution {
                values,
                sample_rates,
                statistic: StatisticKind::Histogram,
            } => {
                // convert distributions into aggregated histograms
                let mut counts = Vec::new();
                for _ in buckets {
                    counts.push(0);
                }
                let mut sum = 0.0;
                let mut count = 0;
                for (v, c) in values.iter().zip(sample_rates.iter()) {
                    buckets
                        .iter()
                        .enumerate()
                        .skip_while(|&(_, b)| b < v)
                        .for_each(|(i, _)| {
                            counts[i] += c;
                        });

                    sum += v * (*c as f64);
                    count += c;
                }

                for (b, c) in buckets.iter().zip(counts.iter()) {
                    s.push_str(&format!(
                        "{}_bucket{} {}\n",
                        fullname,
                        encode_tags_with_extra(tags, "le".to_string(), b.to_string()),
                        c
                    ));
                }
                s.push_str(&format!(
                    "{}_bucket{} {}\n",
                    fullname,
                    encode_tags_with_extra(tags, "le".to_string(), "+Inf".to_string()),
                    count
                ));
                let tags = encode_tags(tags);
                s.push_str(&format!("{}_sum{} {}\n", fullname, tags, sum));
                s.push_str(&format!("{}_count{} {}\n", fullname, tags, count));
            }
            MetricValue::Distribution {
                values,
                sample_rates,
                statistic: StatisticKind::Summary,
            } => {
                if let Some(statistic) = DistributionStatistic::new(values, sample_rates, quantiles)
                {
                    for (q, v) in statistic.quantiles.iter() {
                        s.push_str(&format!(
                            "{}{} {}\n",
                            fullname,
                            encode_tags_with_extra(tags, "quantile".to_string(), q.to_string()),
                            v
                        ));
                    }
                    let tags = encode_tags(tags);
                    s.push_str(&format!("{}_sum{} {}\n", fullname, tags, statistic.sum));
                    s.push_str(&format!("{}_count{} {}\n", fullname, tags, statistic.count));
                    s.push_str(&format!("{}_min{} {}\n", fullname, tags, statistic.min));
                    s.push_str(&format!("{}_max{} {}\n", fullname, tags, statistic.max));
                    s.push_str(&format!("{}_avg{} {}\n", fullname, tags, statistic.avg));
                } else {
                    let tags = encode_tags(tags);
                    s.push_str(&format!("{}_sum{} {}\n", fullname, tags, 0.0));
                    s.push_str(&format!("{}_count{} {}\n", fullname, tags, 0));
                }
            }
            MetricValue::AggregatedHistogram {
                buckets,
                counts,
                count,
                sum,
            } => {
                for (b, c) in buckets.iter().zip(counts.iter()) {
                    s.push_str(&format!(
                        "{}_bucket{} {}\n",
                        fullname,
                        encode_tags_with_extra(tags, "le".to_string(), b.to_string()),
                        c
                    ));
                }
                s.push_str(&format!(
                    "{}_bucket{} {}\n",
                    fullname,
                    encode_tags_with_extra(tags, "le".to_string(), "+Inf".to_string()),
                    count
                ));
                let tags = encode_tags(tags);
                s.push_str(&format!("{}_sum{} {}\n", fullname, tags, sum));
                s.push_str(&format!("{}_count{} {}\n", fullname, tags, count));
            }
            MetricValue::AggregatedSummary {
                quantiles,
                values,
                count,
                sum,
            } => {
                for (q, v) in quantiles.iter().zip(values.iter()) {
                    s.push_str(&format!(
                        "{}{} {}\n",
                        fullname,
                        encode_tags_with_extra(tags, "quantile".to_string(), q.to_string()),
                        v
                    ));
                }
                let tags = encode_tags(tags);
                s.push_str(&format!("{}_sum{} {}\n", fullname, tags, sum));
                s.push_str(&format!("{}_count{} {}\n", fullname, tags, count));
            }
        }
    }

    s
}

fn handle(
    req: Request<Body>,
    namespace: Option<&str>,
    buckets: &[f64],
    quantiles: &[f64],
    expired: bool,
    metrics: &IndexSet<MetricEntry>,
) -> Response<Body> {
    let mut response = Response::new(Body::empty());

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => {
            let mut s = String::new();

            // output headers only once
            let mut processed_headers = HashSet::new();

            for metric in metrics {
                let name = &metric.0.name;
                let frame = encode_metric_datum(namespace, &buckets, quantiles, expired, &metric.0);

                if !processed_headers.contains(&name) {
                    let header = encode_metric_header(namespace, &metric.0);
                    s.push_str(&header);
                    processed_headers.insert(name);
                };

                s.push_str(&frame);
            }

            *response.body_mut() = s.into();

            response.headers_mut().insert(
                "Content-Type",
                HeaderValue::from_static("text/plain; version=0.0.4"),
            );
        }
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    }

    info!(
        message = "Request complete",
        response_code = ?response.status()
    );

    response
}

impl PrometheusSink {
    fn new(config: PrometheusSinkConfig, acker: Acker) -> Self {
        Self {
            server_shutdown_trigger: None,
            config,
            metrics: Arc::new(RwLock::new(IndexSet::new())),
            last_flush_timestamp: Arc::new(RwLock::new(Utc::now().timestamp())),
            acker,
        }
    }

    fn start_server_if_needed(&mut self) {
        if self.server_shutdown_trigger.is_some() {
            return;
        }

        let metrics = Arc::clone(&self.metrics);
        let namespace = self.config.namespace.clone();
        let buckets = self.config.buckets.clone();
        let quantiles = self.config.quantiles.clone();
        let last_flush_timestamp = Arc::clone(&self.last_flush_timestamp);
        let flush_period_secs = self.config.flush_period_secs;

        let new_service = make_service_fn(move |_| {
            let metrics = Arc::clone(&metrics);
            let namespace = namespace.clone();
            let buckets = buckets.clone();
            let quantiles = quantiles.clone();
            let last_flush_timestamp = Arc::clone(&last_flush_timestamp);
            let flush_period_secs = flush_period_secs;

            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let metrics = metrics.read().unwrap();
                    let last_flush_timestamp = last_flush_timestamp.read().unwrap();
                    let interval = (Utc::now().timestamp() - *last_flush_timestamp) as u64;
                    let expired = interval > flush_period_secs;

                    let response = info_span!(
                        "prometheus_server",
                        method = ?req.method(),
                        path = ?req.uri().path(),
                    )
                    .in_scope(|| {
                        handle(
                            req,
                            namespace.as_deref(),
                            &buckets,
                            &quantiles,
                            expired,
                            &metrics,
                        )
                    });

                    future::ok::<_, Infallible>(response)
                }))
            }
        });

        let (trigger, tripwire) = Tripwire::new();

        let server = Server::bind(&self.config.address)
            .serve(new_service)
            .with_graceful_shutdown(tripwire.then(crate::stream::tripwire_handler))
            .map_err(|e| eprintln!("server error: {}", e));

        tokio::spawn(server);
        self.server_shutdown_trigger = Some(trigger);
    }
}

#[async_trait]
impl StreamSink for PrometheusSink {
    async fn run(&mut self, mut input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.start_server_if_needed();
        while let Some(event) = input.next().await {
            let item = event.into_metric();
            let mut metrics = self.metrics.write().unwrap();

            match item.kind {
                MetricKind::Incremental => {
                    let new = MetricEntry(item.to_absolute());
                    if let Some(MetricEntry(mut existing)) = metrics.take(&new) {
                        if item.value.is_set() {
                            // sets need to be expired from time to time
                            // because otherwise they could grow infinitelly
                            let now = Utc::now().timestamp();
                            let interval = now - *self.last_flush_timestamp.read().unwrap();
                            if interval > self.config.flush_period_secs as i64 {
                                *self.last_flush_timestamp.write().unwrap() = now;
                                existing.reset();
                            }
                        }
                        existing.add(&item);
                        metrics.insert(MetricEntry(existing));
                    } else {
                        metrics.insert(new);
                    };
                }
                MetricKind::Absolute => {
                    let new = MetricEntry(item);
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
    use crate::event::metric::{Metric, MetricKind, MetricValue, StatisticKind};
    use pretty_assertions::assert_eq;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<PrometheusSinkConfig>();
    }

    fn tags() -> BTreeMap<String, String> {
        vec![("code".to_owned(), "200".to_owned())]
            .into_iter()
            .collect()
    }

    #[test]
    fn test_encode_counter() {
        let metric = Metric {
            name: "hits".to_owned(),
            timestamp: None,
            tags: Some(tags()),
            kind: MetricKind::Absolute,
            value: MetricValue::Counter { value: 10.0 },
        };

        let header = encode_metric_header(Some("vector"), &metric);
        let frame = encode_metric_datum(Some("vector"), &[], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP vector_hits hits\n# TYPE vector_hits counter\n".to_owned()
        );
        assert_eq!(frame, "vector_hits{code=\"200\"} 10\n".to_owned());
    }

    #[test]
    fn test_encode_gauge() {
        let metric = Metric {
            name: "temperature".to_owned(),
            timestamp: None,
            tags: Some(tags()),
            kind: MetricKind::Absolute,
            value: MetricValue::Gauge { value: -1.1 },
        };

        let header = encode_metric_header(Some("vector"), &metric);
        let frame = encode_metric_datum(Some("vector"), &[], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP vector_temperature temperature\n# TYPE vector_temperature gauge\n".to_owned()
        );
        assert_eq!(frame, "vector_temperature{code=\"200\"} -1.1\n".to_owned());
    }

    #[test]
    fn test_encode_set() {
        let metric = Metric {
            name: "users".to_owned(),
            timestamp: None,
            tags: None,
            kind: MetricKind::Absolute,
            value: MetricValue::Set {
                values: vec!["foo".into()].into_iter().collect(),
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP users users\n# TYPE users gauge\n".to_owned()
        );
        assert_eq!(frame, "users 1\n".to_owned());
    }

    #[test]
    fn test_encode_expired_set() {
        let metric = Metric {
            name: "users".to_owned(),
            timestamp: None,
            tags: None,
            kind: MetricKind::Absolute,
            value: MetricValue::Set {
                values: vec!["foo".into()].into_iter().collect(),
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[], &[], true, &metric);

        assert_eq!(
            header,
            "# HELP users users\n# TYPE users gauge\n".to_owned()
        );
        assert_eq!(frame, "users 0\n".to_owned());
    }

    #[test]
    fn test_encode_distribution() {
        let metric = Metric {
            name: "requests".to_owned(),
            timestamp: None,
            tags: None,
            kind: MetricKind::Absolute,
            value: MetricValue::Distribution {
                values: vec![1.0, 2.0, 3.0],
                sample_rates: vec![3, 3, 2],
                statistic: StatisticKind::Histogram,
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[0.0, 2.5, 5.0], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP requests requests\n# TYPE requests histogram\n".to_owned()
        );
        assert_eq!(frame, "requests_bucket{le=\"0\"} 0\nrequests_bucket{le=\"2.5\"} 6\nrequests_bucket{le=\"5\"} 8\nrequests_bucket{le=\"+Inf\"} 8\nrequests_sum 15\nrequests_count 8\n".to_owned());
    }

    #[test]
    fn test_encode_histogram() {
        let metric = Metric {
            name: "requests".to_owned(),
            timestamp: None,
            tags: None,
            kind: MetricKind::Absolute,
            value: MetricValue::AggregatedHistogram {
                buckets: vec![1.0, 2.1, 3.0],
                counts: vec![1, 2, 3],
                count: 6,
                sum: 12.5,
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP requests requests\n# TYPE requests histogram\n".to_owned()
        );
        assert_eq!(frame, "requests_bucket{le=\"1\"} 1\nrequests_bucket{le=\"2.1\"} 2\nrequests_bucket{le=\"3\"} 3\nrequests_bucket{le=\"+Inf\"} 6\nrequests_sum 12.5\nrequests_count 6\n".to_owned());
    }

    #[test]
    fn test_encode_summary() {
        let metric = Metric {
            name: "requests".to_owned(),
            timestamp: None,
            tags: Some(tags()),
            kind: MetricKind::Absolute,
            value: MetricValue::AggregatedSummary {
                quantiles: vec![0.01, 0.5, 0.99],
                values: vec![1.5, 2.0, 3.0],
                count: 6,
                sum: 12.0,
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[], &[], false, &metric);

        assert_eq!(
            header,
            "# HELP requests requests\n# TYPE requests summary\n".to_owned()
        );
        assert_eq!(frame, "requests{code=\"200\",quantile=\"0.01\"} 1.5\nrequests{code=\"200\",quantile=\"0.5\"} 2\nrequests{code=\"200\",quantile=\"0.99\"} 3\nrequests_sum{code=\"200\"} 12\nrequests_count{code=\"200\"} 6\n".to_owned());
    }

    #[test]
    fn test_encode_distribution_summary() {
        let metric = Metric {
            name: "requests".to_owned(),
            timestamp: None,
            tags: Some(tags()),
            kind: MetricKind::Absolute,
            value: MetricValue::Distribution {
                values: vec![1.0, 2.0, 3.0],
                sample_rates: vec![3, 3, 2],
                statistic: StatisticKind::Summary,
            },
        };

        let header = encode_metric_header(None, &metric);
        let frame = encode_metric_datum(None, &[], &default_summary_quantiles(), false, &metric);

        assert_eq!(
            header,
            "# HELP requests requests\n# TYPE requests summary\n".to_owned()
        );
        assert_eq!(frame, "requests{code=\"200\",quantile=\"0.5\"} 2\nrequests{code=\"200\",quantile=\"0.75\"} 2\nrequests{code=\"200\",quantile=\"0.9\"} 3\nrequests{code=\"200\",quantile=\"0.95\"} 3\nrequests{code=\"200\",quantile=\"0.99\"} 3\nrequests_sum{code=\"200\"} 15\nrequests_count{code=\"200\"} 8\nrequests_min{code=\"200\"} 1\nrequests_max{code=\"200\"} 3\nrequests_avg{code=\"200\"} 1.875\n".to_owned());
    }
}
