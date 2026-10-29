use super::sender::SyslogSender;
use crate::utils::logging::LOG_ERROR_LEVEL_MOD;
use chrono::{DateTime, Local};
use env_filter::{Builder, Filter};
use log::{Level, LevelFilter, Metadata, Record};
use std::sync::Arc;

pub(crate) struct SyslogSink {
    pub(crate) sender: Arc<SyslogSender>,
    pub(crate) filter: Option<Filter>,
    #[cfg(test)]
    pub(crate) offer_count: std::sync::atomic::AtomicU64,
}

impl SyslogSink {
    pub(crate) fn new(sender: Arc<SyslogSender>, filter: Option<Filter>) -> Self {
        Self {
            sender,
            filter,
            #[cfg(test)]
            offer_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn build_filter(level: LevelFilter) -> Filter {
        let mut builder = Builder::new();
        builder.filter_level(level);
        for module in LOG_ERROR_LEVEL_MOD {
            builder.filter_module(module, LevelFilter::Error);
        }
        builder.build()
    }

    pub(crate) fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.as_ref().is_some_and(|f| f.enabled(metadata))
    }

    pub(crate) fn matches(&self, record: &Record<'_>, root_enabled: bool) -> bool {
        self.filter.as_ref().map_or(root_enabled, |f| f.matches(record))
    }

    pub(crate) fn max_level(&self) -> LevelFilter { self.filter.as_ref().map_or(LevelFilter::Off, Filter::filter) }

    pub(crate) fn offer(&self, now: DateTime<Local>, level: Level, target: &str, message: &str) {
        #[cfg(test)]
        self.offer_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sender.offer(now, level, target, message);
    }

    #[cfg(test)]
    pub(crate) fn offer_count(&self) -> u64 { self.offer_count.load(std::sync::atomic::Ordering::Relaxed) }
}
