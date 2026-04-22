use crate::{
    error::Error,
    services::{get_base_href, request_get, request_post},
};
use log::error;
use shared::{
    model::{PlaylistBouquetDto, PlaylistCategoriesDto},
    utils::concat_path_leading_slash,
};
use std::rc::Rc;

fn user_playlist_api_path(base_href: &str, endpoint: &str) -> String {
    concat_path_leading_slash(base_href, &format!("api/v1/user/playlist/{endpoint}"))
}

#[derive(Debug, Default)]
pub struct UserApiService {
    user_playlist_categories_path: String,
    user_playlist_bouquet_path: String,
}

impl UserApiService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            user_playlist_categories_path: user_playlist_api_path(&base_href, "categories"),
            user_playlist_bouquet_path: user_playlist_api_path(&base_href, "bouquet"),
        }
    }

    pub async fn get_playlist_categories(&self) -> Result<Option<Rc<PlaylistCategoriesDto>>, Error> {
        request_get::<Rc<PlaylistCategoriesDto>>(&self.user_playlist_categories_path, None, None)
            .await
            .inspect_err(|err| error!("{err}"))
    }

    pub async fn get_playlist_bouquet(&self) -> Result<Option<Rc<PlaylistBouquetDto>>, Error> {
        request_get::<Rc<PlaylistBouquetDto>>(&self.user_playlist_bouquet_path, None, None)
            .await
            .inspect_err(|err| error!("{err}"))
    }

    pub async fn save_playlist_bouquet(&self, bouquet: &PlaylistBouquetDto) -> Result<(), Error> {
        request_post::<&PlaylistBouquetDto, ()>(&self.user_playlist_bouquet_path, bouquet, None, None)
            .await
            .inspect_err(|err| error!("{err}"))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::user_playlist_api_path;

    #[test]
    fn user_playlist_api_path_builds_expected_bouquet_route() {
        assert_eq!(user_playlist_api_path("", "bouquet"), "/api/v1/user/playlist/bouquet");
        assert_eq!(user_playlist_api_path("/tp", "bouquet"), "/tp/api/v1/user/playlist/bouquet");
    }
}
