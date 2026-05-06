use crate::{
    services::{get_base_href, request_get, Encoding},
    utils::encoding_for_query,
};
use shared::{
    model::{
        PagedResponseDto, QosSnapshotRecordDto, StreamHistoryPageRequestDto, StreamHistoryProviderSummaryDto,
        StreamHistoryRecordDto,
    },
    utils::concat_path_leading_slash,
};

pub struct StreamHistoryService {
    path: String,
    qos_path: String,
}

impl Default for StreamHistoryService {
    fn default() -> Self { Self::new() }
}

impl StreamHistoryService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        let api = |endpoint: &str| concat_path_leading_slash(&base_href, &format!("api/v1/{endpoint}"));
        Self { path: api("stream-history"), qos_path: api("qos-snapshots") }
    }

    pub async fn get_history(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Option<Vec<StreamHistoryRecordDto>>, crate::error::Error> {
        let url = match (from, to) {
            (Some(f), Some(t)) => format!("{}?from={}&to={}", self.path, f, t),
            (Some(f), None) => format!("{}?from={}", self.path, f),
            (None, Some(t)) => format!("{}?to={}", self.path, t),
            (None, None) => self.path.clone(),
        };
        request_get::<Vec<StreamHistoryRecordDto>>(&url, None, Some(Encoding::Cbor)).await
    }

    pub async fn get_summary(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Option<Vec<StreamHistoryProviderSummaryDto>>, crate::error::Error> {
        let summary_path = format!("{}/summary", self.path);
        let url = match (from, to) {
            (Some(f), Some(t)) => format!("{}?from={}&to={}", summary_path, f, t),
            (Some(f), None) => format!("{}?from={}", summary_path, f),
            (None, Some(t)) => format!("{}?to={}", summary_path, t),
            (None, None) => summary_path,
        };
        request_get::<Vec<StreamHistoryProviderSummaryDto>>(&url, None, None).await
    }

    pub async fn get_qos_snapshots(&self) -> Result<Option<Vec<QosSnapshotRecordDto>>, crate::error::Error> {
        request_get::<Vec<QosSnapshotRecordDto>>(&self.qos_path, None, Some(Encoding::Cbor)).await
    }

    pub async fn get_qos_snapshot_detail(
        &self,
        stream_identity_key: &str,
    ) -> Result<Option<QosSnapshotRecordDto>, crate::error::Error> {
        let path = format!("{}/{}", self.qos_path, stream_identity_key);
        request_get::<QosSnapshotRecordDto>(&path, None, Some(Encoding::Cbor)).await
    }

    pub async fn get_history_page(
        &self,
        request: StreamHistoryPageRequestDto,
    ) -> Result<Option<PagedResponseDto<StreamHistoryRecordDto>>, crate::error::Error> {
        let page_path = &self.path;
        let mut params: Vec<(String, String)> = Vec::new();

        // Base parameters
        if let Some(f) = request.from.as_ref() {
            params.push(("from".to_string(), f.clone()));
        }
        if let Some(t) = request.to.as_ref() {
            params.push(("to".to_string(), t.clone()));
        }
        params.push(("page".to_string(), request.page.to_string()));
        params.push(("page_size".to_string(), request.page_size.to_string()));

        // Search parameters
        if let Some(s) = request.search.as_ref() {
            params.push(("search".to_string(), s.clone()));
        }
        if let Some(mode) = request.search_mode.as_ref() {
            params.push(("search_mode".to_string(), mode.clone()));
        }
        if let Some(fields) = request.search_fields.as_ref() {
            for field in fields {
                params.push(("search_field".to_string(), field.clone()));
            }
        }

        // Build URL with proper encoding
        let mut url = format!("{page_path}?");
        for (i, (key, value)) in params.iter().enumerate() {
            // Encode key and value manually
            let enc_key = encoding_for_query(key);
            let enc_value = encoding_for_query(value);
            if i > 0 {
                url.push('&');
            }
            url.push_str(&format!("{enc_key}={enc_value}"));
        }

        request_get::<PagedResponseDto<StreamHistoryRecordDto>>(&url, None, Some(Encoding::Cbor)).await
    }
}
