use std::{collections::BTreeMap, path::Path};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use glob::{Pattern, PatternError};
#[cfg(not(target_os = "windows"))]
use heim::units::ratio::ratio;
use heim::units::time::second;
use tokio::time;
use tokio_stream::wrappers::IntervalStream;
use vector_config::configurable_component;
use vector_core::config::LogNamespace;
use vector_core::ByteSizeOf;

use crate::{
    config::{DataType, Output, SourceConfig, SourceContext, SourceDescription},
    event::metric::{Metric, MetricKind, MetricValue},
    internal_events::{BytesReceived, EventsReceived, StreamClosedError},
    shutdown::ShutdownSignal,
    SourceSender,
};

#[cfg(target_os = "linux")]
mod cgroups;
mod cpu;
mod disk;
mod filesystem;
mod memory;
mod network;

/// Collector types.
#[configurable_component]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Collector {
    /// CGroups.
    #[cfg(target_os = "linux")]
    CGroups,

    /// CPU.
    Cpu,

    /// Disk.
    Disk,

    /// Filesystem.
    Filesystem,

    /// Load average.
    Load,

    /// Host.
    Host,

    /// Memory.
    Memory,

    /// Network.
    Network,
}

/// Filtering configuration.
#[configurable_component]
#[derive(Clone, Debug, Default)]
pub(self) struct FilterList {
    /// Any patterns which should be included.
    includes: Option<Vec<PatternWrapper>>,

    /// Any patterns which should be excluded.
    excludes: Option<Vec<PatternWrapper>>,
}

/// Configuration for the `host_metrics` source.
#[configurable_component(source)]
#[derive(Clone, Debug, Derivative)]
#[derivative(Default)]
#[serde(deny_unknown_fields)]
pub struct HostMetricsConfig {
    /// The interval between metric gathering, in seconds.
    #[serde(default = "default_scrape_interval")]
    pub scrape_interval_secs: f64,

    /// The list of host metric collector services to use.
    ///
    /// Defaults to all collectors.
    pub collectors: Option<Vec<Collector>>,

    /// Overrides the default namespace for the metrics emitted by the source.
    ///
    /// By default, `host` is used.
    #[derivative(Default(value = "default_namespace()"))]
    #[serde(default = "default_namespace")]
    pub namespace: Option<String>,

    #[cfg(target_os = "linux")]
    #[configurable(derived)]
    #[serde(default)]
    pub(crate) cgroups: cgroups::CGroupsConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub disk: disk::DiskConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub filesystem: filesystem::FilesystemConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub network: network::NetworkConfig,
}

const fn default_scrape_interval() -> f64 {
    15.0
}

fn default_namespace() -> Option<String> {
    Some(String::from("host"))
}

inventory::submit! {
    SourceDescription::new::<HostMetricsConfig>("host_metrics")
}

impl_generate_config_from_default!(HostMetricsConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "host_metrics")]
impl SourceConfig for HostMetricsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        init_roots();

        let mut config = self.clone();
        config.namespace = config.namespace.filter(|namespace| !namespace.is_empty());

        Ok(Box::pin(config.run(cx.out, cx.shutdown)))
    }

    fn outputs(&self, _global_log_namespace: LogNamespace) -> Vec<Output> {
        vec![Output::default(DataType::Metric)]
    }

    fn source_type(&self) -> &'static str {
        "host_metrics"
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

impl HostMetricsConfig {
    /// Set the interval to collect internal metrics.
    pub fn scrape_interval_secs(&mut self, value: f64) {
        self.scrape_interval_secs = value;
    }

    async fn run(self, mut out: SourceSender, shutdown: ShutdownSignal) -> Result<(), ()> {
        let duration = time::Duration::from_secs_f64(self.scrape_interval_secs);
        let mut interval = IntervalStream::new(time::interval(duration)).take_until(shutdown);

        let generator = HostMetrics::new(self);

        while interval.next().await.is_some() {
            emit!(BytesReceived {
                byte_size: 0,
                protocol: "none"
            });
            let metrics = generator.capture_metrics().await;
            let count = metrics.len();
            if let Err(error) = out.send_batch(metrics).await {
                emit!(StreamClosedError {
                    count,
                    error: error.clone()
                });
                error!(message = "Error sending host metrics.", %error);
                return Err(());
            }
        }

        Ok(())
    }

    fn has_collector(&self, collector: Collector) -> bool {
        match &self.collectors {
            None => true,
            Some(collectors) => collectors.iter().any(|&c| c == collector),
        }
    }
}

pub struct HostMetrics {
    config: HostMetricsConfig,
    #[cfg(target_os = "linux")]
    root_cgroup: Option<cgroups::CGroup>,
}

impl HostMetrics {
    #[cfg(not(target_os = "linux"))]
    pub const fn new(config: HostMetricsConfig) -> Self {
        Self { config }
    }

    #[cfg(target_os = "linux")]
    pub fn new(config: HostMetricsConfig) -> Self {
        let root_cgroup = cgroups::CGroup::root(config.cgroups.base.as_deref());
        Self {
            config,
            root_cgroup,
        }
    }

    async fn capture_metrics(&self) -> Vec<Metric> {
        let hostname = crate::get_hostname();

        let mut metrics = Vec::new();
        #[cfg(target_os = "linux")]
        if self.config.has_collector(Collector::CGroups) {
            metrics.extend(add_collector("cgroups", self.cgroups_metrics().await));
        }
        if self.config.has_collector(Collector::Cpu) {
            metrics.extend(add_collector("cpu", self.cpu_metrics().await));
        }
        if self.config.has_collector(Collector::Disk) {
            metrics.extend(add_collector("disk", self.disk_metrics().await));
        }
        if self.config.has_collector(Collector::Filesystem) {
            metrics.extend(add_collector("filesystem", self.filesystem_metrics().await));
        }
        if self.config.has_collector(Collector::Load) {
            metrics.extend(add_collector("load", self.loadavg_metrics().await));
        }
        if self.config.has_collector(Collector::Host) {
            metrics.extend(add_collector("host", self.host_metrics().await));
        }
        if self.config.has_collector(Collector::Memory) {
            metrics.extend(add_collector("memory", self.memory_metrics().await));
            metrics.extend(add_collector("memory", self.swap_metrics().await));
        }
        if self.config.has_collector(Collector::Network) {
            metrics.extend(add_collector("network", self.network_metrics().await));
        }
        if let Ok(hostname) = &hostname {
            for metric in &mut metrics {
                metric.insert_tag("host".into(), hostname.into());
            }
        }
        emit!(EventsReceived {
            count: metrics.len(),
            byte_size: metrics.size_of(),
        });
        metrics
    }

    pub async fn loadavg_metrics(&self) -> Vec<Metric> {
        #[cfg(unix)]
        let result = match heim::cpu::os::unix::loadavg().await {
            Ok(loadavg) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "load1",
                        timestamp,
                        loadavg.0.get::<ratio>() as f64,
                        BTreeMap::new(),
                    ),
                    self.gauge(
                        "load5",
                        timestamp,
                        loadavg.1.get::<ratio>() as f64,
                        BTreeMap::new(),
                    ),
                    self.gauge(
                        "load15",
                        timestamp,
                        loadavg.2.get::<ratio>() as f64,
                        BTreeMap::new(),
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load load average info.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        };
        #[cfg(not(unix))]
        let result = vec![];

        result
    }

    pub async fn host_metrics(&self) -> Vec<Metric> {
        let mut metrics = Vec::new();
        match heim::host::uptime().await {
            Ok(time) => {
                let timestamp = Utc::now();
                metrics.push(self.gauge(
                    "uptime",
                    timestamp,
                    time.get::<second>() as f64,
                    BTreeMap::default(),
                ));
            }
            Err(error) => {
                error!(message = "Failed to load host uptime info.", %error, internal_log_rate_secs = 60);
            }
        }

        match heim::host::boot_time().await {
            Ok(time) => {
                let timestamp = Utc::now();
                metrics.push(self.gauge(
                    "boot_time",
                    timestamp,
                    time.get::<second>() as f64,
                    BTreeMap::default(),
                ));
            }
            Err(error) => {
                error!(message = "Failed to load host boot time info.", %error, internal_log_rate_secs = 60);
            }
        }

        metrics
    }

    fn counter(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric::new(name, MetricKind::Absolute, MetricValue::Counter { value })
            .with_namespace(self.config.namespace.clone())
            .with_tags(Some(tags))
            .with_timestamp(Some(timestamp))
    }

    fn gauge(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric::new(name, MetricKind::Absolute, MetricValue::Gauge { value })
            .with_namespace(self.config.namespace.clone())
            .with_tags(Some(tags))
            .with_timestamp(Some(timestamp))
    }
}

pub(self) fn filter_result_sync<T, E>(result: Result<T, E>, message: &'static str) -> Option<T>
where
    E: std::error::Error,
{
    result
        .map_err(|error| error!(message, %error, internal_log_rate_secs = 60))
        .ok()
}

pub(self) async fn filter_result<T, E>(result: Result<T, E>, message: &'static str) -> Option<T>
where
    E: std::error::Error,
{
    filter_result_sync(result, message)
}

fn add_collector(collector: &str, mut metrics: Vec<Metric>) -> Vec<Metric> {
    for metric in &mut metrics {
        metric.insert_tag("collector".into(), collector.into());
    }
    metrics
}

#[allow(clippy::missing_const_for_fn)]
fn init_roots() {
    #[cfg(target_os = "linux")]
    {
        use std::sync::Once;

        static INIT: Once = Once::new();

        INIT.call_once(|| {
            match std::env::var_os("PROCFS_ROOT") {
                Some(procfs_root) => {
                    info!(
                        message = "PROCFS_ROOT is set in envvars. Using custom for procfs.",
                        custom = ?procfs_root
                    );
                    heim::os::linux::set_procfs_root(std::path::PathBuf::from(&procfs_root));
                }
                None => info!("PROCFS_ROOT is unset. Using default '/proc' for procfs root."),
            };

            match std::env::var_os("SYSFS_ROOT") {
                Some(sysfs_root) => {
                    info!(
                        message = "SYSFS_ROOT is set in envvars. Using custom for sysfs.",
                        custom = ?sysfs_root
                    );
                    heim::os::linux::set_sysfs_root(std::path::PathBuf::from(&sysfs_root));
                }
                None => info!("SYSFS_ROOT is unset. Using default '/sys' for sysfs root."),
            }
        });
    };
}

impl FilterList {
    fn contains<T, M>(&self, value: &Option<T>, matches: M) -> bool
    where
        M: Fn(&PatternWrapper, &T) -> bool,
    {
        (match (&self.includes, value) {
            // No includes list includes everything
            (None, _) => true,
            // Includes list matched against empty value returns false
            (Some(_), None) => false,
            // Otherwise find the given value
            (Some(includes), Some(value)) => includes.iter().any(|pattern| matches(pattern, value)),
        }) && match (&self.excludes, value) {
            // No excludes, list excludes nothing
            (None, _) => true,
            // No value, never excluded
            (Some(_), None) => true,
            // Otherwise find the given value
            (Some(excludes), Some(value)) => {
                !excludes.iter().any(|pattern| matches(pattern, value))
            }
        }
    }

    fn contains_str(&self, value: Option<&str>) -> bool {
        self.contains(&value, |pattern, s| pattern.matches_str(s))
    }

    fn contains_path(&self, value: Option<&Path>) -> bool {
        self.contains(&value, |pattern, path| pattern.matches_path(path))
    }

    #[cfg(test)]
    fn contains_test(&self, value: Option<&str>) -> bool {
        let result = self.contains_str(value);
        assert_eq!(result, self.contains_path(value.map(std::path::Path::new)));
        result
    }
}

/// A compiled Unix shell-style pattern.
///
/// - `?` matches any single character.
/// - `*` matches any (possibly empty) sequence of characters.
/// - `**` matches the current directory and arbitrary subdirectories. This sequence must form a single path component,
///   so both `**a` and `b**` are invalid and will result in an error. A sequence of more than two consecutive `*`
///   characters is also invalid.
/// - `[...]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as
///   ordered by Unicode, so e.g. `[0-9]` specifies any character between 0 and 9 inclusive. An unclosed bracket is
///   invalid.
/// - `[!...]` is the negation of `[...]`, i.e. it matches any characters not in the brackets.
///
/// The metacharacters `?`, `*`, `[`, `]` can be matched by using brackets (e.g. `[?]`). When a `]` occurs immediately
/// following `[` or `[!` then it is interpreted as being part of, rather then ending, the character set, so `]` and NOT
/// `]` can be matched by `[]]` and `[!]]` respectively. The `-` character can be specified inside a character sequence
/// pattern by placing it at the start or the end, e.g. `[abc-]`.
#[configurable_component]
#[derive(Clone, Debug)]
#[serde(try_from = "String", into = "String")]
struct PatternWrapper(Pattern);

impl PatternWrapper {
    fn matches_str(&self, s: &str) -> bool {
        self.0.matches(s)
    }

    fn matches_path(&self, p: &Path) -> bool {
        self.0.matches_path(p)
    }
}

impl TryFrom<String> for PatternWrapper {
    type Error = PatternError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Pattern::new(value.as_ref()).map(PatternWrapper)
    }
}

impl From<PatternWrapper> for String {
    fn from(pattern: PatternWrapper) -> Self {
        pattern.0.to_string()
    }
}

#[cfg(test)]
pub(self) mod tests {
    use std::{collections::HashSet, future::Future};

    use super::*;

    #[test]
    fn filterlist_default_includes_everything() {
        let filters = FilterList::default();
        assert!(filters.contains_test(Some("anything")));
        assert!(filters.contains_test(Some("should")));
        assert!(filters.contains_test(Some("work")));
        assert!(filters.contains_test(None));
    }

    #[test]
    fn filterlist_includes_works() {
        let filters = FilterList {
            includes: Some(vec![
                PatternWrapper::try_from("sda".to_string()).unwrap(),
                PatternWrapper::try_from("dm-*".to_string()).unwrap(),
            ]),
            excludes: None,
        };
        assert!(!filters.contains_test(Some("sd")));
        assert!(filters.contains_test(Some("sda")));
        assert!(!filters.contains_test(Some("sda1")));
        assert!(filters.contains_test(Some("dm-")));
        assert!(filters.contains_test(Some("dm-5")));
        assert!(!filters.contains_test(Some("xda")));
        assert!(!filters.contains_test(None));
    }

    #[test]
    fn filterlist_excludes_works() {
        let filters = FilterList {
            includes: None,
            excludes: Some(vec![
                PatternWrapper::try_from("sda".to_string()).unwrap(),
                PatternWrapper::try_from("dm-*".to_string()).unwrap(),
            ]),
        };
        assert!(filters.contains_test(Some("sd")));
        assert!(!filters.contains_test(Some("sda")));
        assert!(filters.contains_test(Some("sda1")));
        assert!(!filters.contains_test(Some("dm-")));
        assert!(!filters.contains_test(Some("dm-5")));
        assert!(filters.contains_test(Some("xda")));
        assert!(filters.contains_test(None));
    }

    #[test]
    fn filterlist_includes_and_excludes_works() {
        let filters = FilterList {
            includes: Some(vec![
                PatternWrapper::try_from("sda".to_string()).unwrap(),
                PatternWrapper::try_from("dm-*".to_string()).unwrap(),
            ]),
            excludes: Some(vec![PatternWrapper::try_from("dm-5".to_string()).unwrap()]),
        };
        assert!(!filters.contains_test(Some("sd")));
        assert!(filters.contains_test(Some("sda")));
        assert!(!filters.contains_test(Some("sda1")));
        assert!(filters.contains_test(Some("dm-")));
        assert!(filters.contains_test(Some("dm-1")));
        assert!(!filters.contains_test(Some("dm-5")));
        assert!(!filters.contains_test(Some("xda")));
        assert!(!filters.contains_test(None));
    }

    #[tokio::test]
    async fn filters_on_collectors() {
        let all_metrics_count = HostMetrics::new(HostMetricsConfig::default())
            .capture_metrics()
            .await
            .len();

        for collector in &[
            #[cfg(target_os = "linux")]
            Collector::CGroups,
            Collector::Cpu,
            Collector::Disk,
            Collector::Filesystem,
            Collector::Load,
            Collector::Host,
            Collector::Memory,
            Collector::Network,
        ] {
            let some_metrics = HostMetrics::new(HostMetricsConfig {
                collectors: Some(vec![*collector]),
                ..Default::default()
            })
            .capture_metrics()
            .await;

            assert!(
                all_metrics_count > some_metrics.len(),
                "collector={:?}",
                collector
            );
        }
    }

    #[tokio::test]
    async fn are_tagged_with_hostname() {
        let metrics = HostMetrics::new(HostMetricsConfig::default())
            .capture_metrics()
            .await;
        let hostname = crate::get_hostname().expect("Broken hostname");
        assert!(!metrics.into_iter().any(|event| event
            .tags()
            .expect("Missing tags")
            .get("host")
            .expect("Missing \"host\" tag")
            != &hostname));
    }

    #[tokio::test]
    async fn uses_custom_namespace() {
        let metrics = HostMetrics::new(HostMetricsConfig {
            namespace: Some("other".into()),
            ..Default::default()
        })
        .capture_metrics()
        .await;

        assert!(metrics
            .into_iter()
            .all(|event| event.namespace() == Some("other")));
    }

    #[tokio::test]
    async fn uses_default_namespace() {
        let metrics = HostMetrics::new(HostMetricsConfig::default())
            .capture_metrics()
            .await;

        assert!(metrics
            .iter()
            .all(|event| event.namespace() == Some("host")));
    }

    // Windows does not produce load average metrics.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn generates_loadavg_metrics() {
        let metrics = HostMetrics::new(HostMetricsConfig::default())
            .loadavg_metrics()
            .await;
        assert_eq!(metrics.len(), 3);
        assert!(all_gauges(&metrics));

        // All metrics are named load*
        assert!(!metrics
            .iter()
            .any(|metric| !metric.name().starts_with("load")));
    }

    #[tokio::test]
    async fn generates_host_metrics() {
        let metrics = HostMetrics::new(HostMetricsConfig::default())
            .host_metrics()
            .await;
        assert_eq!(metrics.len(), 2);
        assert!(all_gauges(&metrics));
    }

    pub(super) fn all_counters(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value(), &MetricValue::Counter { .. }))
    }

    pub(super) fn all_gauges(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value(), &MetricValue::Gauge { .. }))
    }

    fn all_tags_match(metrics: &[Metric], tag: &str, matches: impl Fn(&str) -> bool) -> bool {
        !metrics.iter().any(|metric| {
            metric
                .tags()
                .unwrap()
                .get(tag)
                .map(|value| !matches(value))
                .unwrap_or(false)
        })
    }

    pub(super) fn count_name(metrics: &[Metric], name: &str) -> usize {
        metrics
            .iter()
            .filter(|metric| metric.name() == name)
            .count()
    }

    pub(super) fn count_tag(metrics: &[Metric], tag: &str) -> usize {
        metrics
            .iter()
            .filter(|metric| {
                metric
                    .tags()
                    .expect("Metric is missing tags")
                    .contains_key(tag)
            })
            .count()
    }

    fn collect_tag_values(metrics: &[Metric], tag: &str) -> HashSet<String> {
        metrics
            .iter()
            .filter_map(|metric| metric.tags().unwrap().get(tag).cloned())
            .collect::<HashSet<_>>()
    }

    // Run a series of tests using filters to ensure they are obeyed
    pub(super) async fn assert_filtered_metrics<'a, Get, Fut>(tag: &str, get_metrics: Get)
    where
        Get: Fn(FilterList) -> Fut,
        Fut: Future<Output = Vec<Metric>>,
    {
        let all_metrics = get_metrics(FilterList::default()).await;
        let keys = collect_tag_values(&all_metrics, tag);
        // Pick an arbitrary key value
        if let Some(key) = keys.into_iter().next() {
            let key_prefix = &key[..key.len() - 1].to_string();
            let key_prefix_pattern = PatternWrapper::try_from(format!("{}*", key_prefix)).unwrap();
            let key_pattern = PatternWrapper::try_from(key.clone()).unwrap();

            let filtered_metrics_with = get_metrics(FilterList {
                includes: Some(vec![key_pattern.clone()]),
                excludes: None,
            })
            .await;

            assert!(filtered_metrics_with.len() <= all_metrics.len());
            assert!(!filtered_metrics_with.is_empty());
            assert!(all_tags_match(&filtered_metrics_with, tag, |s| s == key));

            let filtered_metrics_with_match = get_metrics(FilterList {
                includes: Some(vec![key_prefix_pattern.clone()]),
                excludes: None,
            })
            .await;

            assert!(filtered_metrics_with_match.len() >= filtered_metrics_with.len());
            assert!(all_tags_match(&filtered_metrics_with_match, tag, |s| {
                s.starts_with(key_prefix)
            }));

            let filtered_metrics_without = get_metrics(FilterList {
                includes: None,
                excludes: Some(vec![key_pattern]),
            })
            .await;

            assert!(filtered_metrics_without.len() <= all_metrics.len());
            assert!(all_tags_match(&filtered_metrics_without, tag, |s| s != key));

            let filtered_metrics_without_match = get_metrics(FilterList {
                includes: None,
                excludes: Some(vec![key_prefix_pattern]),
            })
            .await;

            assert!(filtered_metrics_without_match.len() <= filtered_metrics_without.len());
            assert!(all_tags_match(&filtered_metrics_without_match, tag, |s| {
                !s.starts_with(key_prefix)
            }));

            assert!(
                filtered_metrics_with.len() + filtered_metrics_without.len() <= all_metrics.len()
            );
        }
    }
}
