use crate::{
    error::Error,
    services::{get_base_href, request_delete_meta, request_get, request_get_meta, request_put_meta, ResponseMeta},
};
use shared::{
    model::{TableLayoutPreferencesDto, UserSettingsDto},
    utils::concat_path_leading_slash,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TableLayoutSection {
    pub layout: TableLayoutPreferencesDto,
    pub etag: String,
}

pub struct UserSettingsService {
    path: String,
}
impl Default for UserSettingsService {
    fn default() -> Self { Self::new() }
}
impl UserSettingsService {
    pub fn new() -> Self { Self { path: concat_path_leading_slash(&get_base_href(), "api/v1/me/settings") } }
    pub async fn load(&self) -> Result<UserSettingsDto, Error> {
        request_get(&self.path, None, None).await?.ok_or(Error::DeserializeError)
    }
    fn table_path(&self, table: &str) -> String {
        format!("{}/tables/{}", self.path, js_sys::encode_uri_component(table))
    }
    fn section(meta: ResponseMeta<TableLayoutPreferencesDto>) -> Result<TableLayoutSection, Error> {
        Ok(TableLayoutSection {
            layout: meta.body.ok_or(Error::DeserializeError)?,
            etag: meta.headers.get("etag").cloned().ok_or(Error::DeserializeError)?,
        })
    }
    pub async fn table(&self, table: &str) -> Result<TableLayoutSection, Error> {
        Self::section(request_get_meta(&self.table_path(table), None, None, &["etag"]).await?)
    }
    pub async fn save(
        &self,
        table: &str,
        layout: TableLayoutPreferencesDto,
        etag: &str,
    ) -> Result<TableLayoutSection, Error> {
        Self::section(
            request_put_meta(
                &self.table_path(table),
                layout,
                None,
                None,
                Some(&[("If-Match".into(), etag.into())]),
                &["etag"],
            )
            .await?,
        )
    }
    pub async fn reset(&self, table: &str, etag: &str) -> Result<TableLayoutSection, Error> {
        Self::section(
            request_delete_meta(&self.table_path(table), &[("If-Match".into(), etag.into())], &["etag"]).await?,
        )
    }
}
