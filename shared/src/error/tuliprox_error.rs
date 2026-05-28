use crate::utils::sanitize_sensitive_info;
use thiserror::Error;

#[macro_export]
macro_rules! get_errors_notify_message {
    ($errors:expr, $size:expr) => {
        if $errors.is_empty() {
            None
        } else {
            let text = $errors
                .iter()
                .filter(|err| err.is_notify())
                .map(|err| err.message())
                .collect::<Vec<&str>>()
                .join("\r\n");

            let max_size = $size;
            if max_size > 0 && text.chars().count() > max_size {
                let truncated: String = text.chars().take(max_size).collect();
                Some(format!("{}...", truncated))
            } else if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
    };
}

pub use get_errors_notify_message;

#[macro_export]
macro_rules! handle_tuliprox_error_result_list {
    ($result: expr) => {
        let errors = $result
            .filter_map(|result| if let Err(err) = result { Some(err.to_string()) } else { None })
            .collect::<Vec<String>>();
        if !errors.is_empty() {
            return Err($crate::error::TuliproxError::Errors(errors.join("\n")));
        }
    };
}

pub use handle_tuliprox_error_result_list;

// #[macro_export]
// macro_rules! handle_tuliprox_error_result {
//     ($result: expr) => {
//         if let Err(err) = $result {
//             return Err($crate::error::TuliproxError::Config(err.to_string()));
//         }
//     };
// }
// pub use handle_tuliprox_error_result;

#[derive(Debug, Error)]
pub enum TuliproxError {
    #[error("errors: {0}")]
    Errors(String),

    #[error("api xtream error: {0}")]
    ApiXtream(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("config app error: {0}")]
    ConfigApp(String),

    #[error("config cache error: {0}")]
    ConfigCache(String),

    #[error("config base error: {0}")]
    ConfigBase(String),

    #[error("config API proxy error: {0}")]
    ConfigApiProxy(String),

    #[error("config EPG error: {0}")]
    ConfigEpg(String),

    #[error("config HDHomeRun error: {0}")]
    ConfigHdhomerun(String),

    #[error("config input error: {0}")]
    ConfigInput(String),

    #[error("config IP check error: {0}")]
    ConfigIpCheck(String),

    #[error("config library error: {0}")]
    ConfigLibrary(String),

    #[error("config metadata update error: {0}")]
    ConfigMetadataUpdate(String),

    #[error("config panel API error: {0}")]
    ConfigPanelApi(String),

    #[error("config proxy error: {0}")]
    ConfigProxy(String),

    #[error("config proxy type error: {0}")]
    ConfigProxyType(String),

    #[error("config proxy user status error: {0}")]
    ConfigProxyUserStatus(String),

    #[error("config QoS aggregation error: {0}")]
    ConfigQosAggregation(String),

    #[error("config rate limit error: {0}")]
    ConfigRateLimit(String),

    #[error("config reverse proxy error: {0}")]
    ConfigReverseProxy(String),

    #[error("config sort error: {0}")]
    ConfigSort(String),

    #[error("config source error: {0}")]
    ConfigSource(String),

    #[error("config stream error: {0}")]
    ConfigStream(String),

    #[error("config stream history error: {0}")]
    ConfigStreamHistory(String),

    #[error("config target error: {0}")]
    ConfigTarget(String),

    #[error("config web UI error: {0}")]
    ConfigWebUi(String),

    #[error("repository EPG error: {0}")]
    RepositoryEpg(String),

    #[error("repository XTream error: {0}")]
    RepositoryXtream(String),

    #[error("repository M3U error: {0}")]
    RepositoryM3u(String),

    #[error("repository Library error: {0}")]
    RepositoryLibrary(String),

    #[error("repository Storage error: {0}")]
    RepositoryStorage(String),

    #[error("repository Network error: {0}")]
    RepositoryNetwork(String),

    #[error("repository Playlist error: {0}")]
    RepositoryPlaylist(String),

    #[error("repository Trakt error: {0}")]
    RepositoryTrakt(String),

    #[error("filter parse error: {0}")]
    FilterParse(String),

    #[error("mapper error: {0}")]
    Mapper(String),

    #[error("regex compile error: {0}")]
    RegexCompile(String),

    #[error("url parse error: {0}")]
    UrlParse(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("server error: {0}")]
    Server(String),

    #[error("probe error: {0}")]
    Probe(String),

    #[error("task error: {0}")]
    Task(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("proxy user error: {0}")]
    ProxyUser(String),

    #[error("repository error: {0}")]
    Repository(String),

    #[error("download error: {0}")]
    Download(String),

    #[error("provider connection error: {0}")]
    ProviderConnection(String),
}

impl TuliproxError {
    pub fn message(&self) -> &str {
        match self {
            Self::Errors(msg)
            | Self::Config(msg)
            | Self::ConfigApp(msg)
            | Self::ConfigCache(msg)
            | Self::ConfigBase(msg)
            | Self::ConfigApiProxy(msg)
            | Self::ConfigEpg(msg)
            | Self::ConfigHdhomerun(msg)
            | Self::ConfigInput(msg)
            | Self::ConfigIpCheck(msg)
            | Self::ConfigLibrary(msg)
            | Self::ConfigMetadataUpdate(msg)
            | Self::ConfigPanelApi(msg)
            | Self::ConfigProxy(msg)
            | Self::ConfigProxyType(msg)
            | Self::ConfigProxyUserStatus(msg)
            | Self::ConfigQosAggregation(msg)
            | Self::ConfigRateLimit(msg)
            | Self::ConfigReverseProxy(msg)
            | Self::ConfigSort(msg)
            | Self::ConfigSource(msg)
            | Self::ConfigStream(msg)
            | Self::ConfigStreamHistory(msg)
            | Self::ConfigTarget(msg)
            | Self::ConfigWebUi(msg)
            | Self::RepositoryEpg(msg)
            | Self::RepositoryXtream(msg)
            | Self::RepositoryM3u(msg)
            | Self::RepositoryLibrary(msg)
            | Self::RepositoryStorage(msg)
            | Self::RepositoryNetwork(msg)
            | Self::RepositoryPlaylist(msg)
            | Self::RepositoryTrakt(msg)
            | Self::FilterParse(msg)
            | Self::Mapper(msg)
            | Self::RegexCompile(msg)
            | Self::UrlParse(msg)
            | Self::Io(msg)
            | Self::Crypto(msg)
            | Self::Server(msg)
            | Self::Probe(msg)
            | Self::Task(msg)
            | Self::Parse(msg)
            | Self::ProxyUser(msg)
            | Self::Repository(msg)
            | Self::Download(msg)
            | Self::ProviderConnection(msg)
            | Self::ApiXtream(msg) => msg,
        }
    }

    pub fn is_notify(&self) -> bool {
        matches!(
            self,
            Self::Repository(_)
                | Self::RepositoryEpg(_)
                | Self::RepositoryXtream(_)
                | Self::RepositoryM3u(_)
                | Self::RepositoryLibrary(_)
                | Self::RepositoryStorage(_)
                | Self::RepositoryNetwork(_)
                | Self::RepositoryPlaylist(_)
                | Self::RepositoryTrakt(_)
                | Self::Download(_)
                | Self::ProviderConnection(_)
        )
    }
}

pub fn to_io_error<E>(err: E) -> std::io::Error
where
    E: std::error::Error,
{
    std::io::Error::other(sanitize_sensitive_info(&err.to_string()))
}

pub fn str_to_io_error(err: &str) -> std::io::Error { std::io::Error::other(sanitize_sensitive_info(err)) }

pub fn string_to_io_error(err: impl AsRef<str>) -> std::io::Error {
    std::io::Error::other(sanitize_sensitive_info(err.as_ref()))
}
