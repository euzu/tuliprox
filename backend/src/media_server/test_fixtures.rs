pub const EMBY_ITEMS_PAGE_JSON: &str = r#"
{
  "Items": [
    {
      "Id": "item-redacted-1",
      "Name": "Movie Redacted",
      "Type": "Movie",
      "ProductionYear": 2024,
      "ProviderIds": { "Tmdb": "12345" },
      "ImageTags": { "Primary": "image-tag-redacted" },
      "Path": "/redacted/upstream/path/movie.mkv",
      "UserData": { "Played": true, "PlaybackPositionTicks": 123 },
      "MediaSources": [
        {
          "Id": "media-source-redacted-1",
          "Container": "mkv",
          "Path": "/redacted/upstream/path/movie.mkv",
          "SupportsDirectPlay": true,
          "SupportsDirectStream": true
        }
      ]
    }
  ],
  "TotalRecordCount": 1,
  "StartIndex": 0
}
"#;

pub const JELLYFIN_VIEWS_JSON: &str = r#"
{
  "Items": [
    { "Id": "library-redacted-movies", "Name": "Movies", "CollectionType": "movies", "Type": "CollectionFolder" },
    { "Id": "library-redacted-tv", "Name": "TV", "CollectionType": "tvshows", "Type": "CollectionFolder" }
  ],
  "TotalRecordCount": 2,
  "StartIndex": 0
}
"#;

pub const PLEX_RESOURCES_XML: &str = r#"
<MediaContainer size="1">
  <Device name="Server Redacted" product="Plex Media Server" productVersion="1.0.0" clientIdentifier="client-redacted" machineIdentifier="machine-redacted" owned="0" accessToken="resource-token-redacted">
    <Connection protocol="https" uri="https://pms.example.invalid" local="0" relay="0" />
  </Device>
</MediaContainer>
"#;

pub const PLEX_SECTIONS_XML: &str = r#"
<MediaContainer size="3">
  <Directory key="1" title="Movies" type="movie" />
  <Directory key="2" title="Shows" type="show" />
  <Directory key="3" title="Music" type="artist" />
</MediaContainer>
"#;

pub const PLEX_MOVIES_XML: &str = r#"
<MediaContainer size="1" totalSize="1">
  <Video ratingKey="rating-redacted-1" key="/library/metadata/rating-redacted-1" type="movie" title="Movie Redacted" year="2024" thumb="/library/metadata/rating-redacted-1/thumb" addedAt="1700000000" updatedAt="1700000001">
    <Guid id="tmdb://12345" />
    <Media id="media-redacted-1" container="mkv" duration="7200000" bitrate="8000" width="1920" height="1080">
      <Part id="part-redacted" key="/library/parts/part-redacted/file.mkv" file="/redacted/upstream/path/movie.mkv" size="1024" container="mkv" />
    </Media>
  </Video>
</MediaContainer>
"#;
