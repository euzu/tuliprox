pub(crate) mod formatter;
pub(crate) mod hostname;
pub(crate) mod sender;
pub(crate) mod sink;

#[cfg(test)]
pub(crate) use formatter::{SyslogHeader, SyslogMeta};
#[cfg(test)]
pub(crate) use sender::Seams;
pub(crate) use sender::{SyslogSender, SyslogTransport};
pub(crate) use sink::SyslogSink;
use std::cell::Cell;

thread_local! {
    pub(crate) static IN_SYSLOG_WORKER: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn in_syslog_worker() -> bool { IN_SYSLOG_WORKER.with(Cell::get) }

pub(crate) struct SyslogWorkerGuard;

impl SyslogWorkerGuard {
    pub(crate) fn enter() -> Self {
        IN_SYSLOG_WORKER.with(|cell| cell.set(true));
        Self
    }
}

impl Drop for SyslogWorkerGuard {
    fn drop(&mut self) { IN_SYSLOG_WORKER.with(|cell| cell.set(false)); }
}
