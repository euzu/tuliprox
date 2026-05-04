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

pub const PLEX_IDENTITY_XML: &str = r#"
<MediaContainer machineIdentifier="machine-redacted" friendlyName="Server Redacted" version="1.0.0" />
"#;

pub const PLEX_RESOURCES_XML: &str = r#"
<MediaContainer size="1">
  <Device name="Server Redacted" product="Plex Media Server" productVersion="1.0.0" clientIdentifier="client-redacted" machineIdentifier="machine-redacted" owned="0" accessToken="resource-token-redacted">
    <Connection protocol="https" uri="https://pms.example.invalid" local="0" relay="0" />
  </Device>
</MediaContainer>
"#;

pub const PLEX_RESOURCES_WITH_RELAY_XML: &str = r#"
<MediaContainer size="1">
  <Device name="Server Redacted" product="Plex Media Server" productVersion="1.0.0" clientIdentifier="client-redacted" machineIdentifier="machine-redacted" owned="0" accessToken="resource-token-redacted">
    <Connection protocol="https" uri="https://relay.example.invalid" local="0" relay="1" />
    <Connection protocol="http" uri="http://pms.example.invalid" local="0" relay="0" />
  </Device>
</MediaContainer>
"#;

pub const PLEX_AMBIGUOUS_RESOURCES_XML: &str = r#"
<MediaContainer size="2">
  <Device name="Duplicated Server" product="Plex Media Server" clientIdentifier="client-redacted-1" machineIdentifier="machine-redacted-1" owned="0" accessToken="resource-token-redacted-1">
    <Connection protocol="https" uri="https://pms-one.example.invalid" local="0" relay="0" />
  </Device>
  <Device name="Duplicated Server" product="Plex Media Server" clientIdentifier="client-redacted-2" machineIdentifier="machine-redacted-2" owned="0" accessToken="resource-token-redacted-2">
    <Connection protocol="https" uri="https://pms-two.example.invalid" local="0" relay="0" />
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

pub const PLEX_MOVIES_WITH_MALFORMED_ROW_XML: &str = r#"
<MediaContainer size="2" totalSize="2">
  <Video key="/library/metadata/missing-rating" type="movie" title="Malformed Movie Redacted">
    <Media id="media-redacted-bad" container="mkv">
      <Part id="part-redacted-bad" key="/library/parts/part-redacted-bad/file.mkv" file="/redacted/upstream/path/bad.mkv" />
    </Media>
  </Video>
  <Video ratingKey="rating-redacted-2" key="/library/metadata/rating-redacted-2" type="movie" title="Movie Redacted 2">
    <Media id="media-redacted-2" container="mkv">
      <Part id="part-redacted-2" key="/library/parts/part-redacted-2/file.mkv" file="/redacted/upstream/path/movie2.mkv" />
    </Media>
  </Video>
</MediaContainer>
"#;

pub const PLEX_EPISODES_XML: &str = r#"
<MediaContainer size="1" totalSize="1">
  <Video ratingKey="episode-redacted-1" key="/library/metadata/episode-redacted-1" type="episode" title="Episode Redacted" grandparentRatingKey="series-redacted-1" grandparentTitle="Series Redacted" parentIndex="1" index="2" thumb="/library/metadata/episode-redacted-1/thumb" addedAt="1700000100" updatedAt="1700000101">
    <Guid id="tvdb://67890" />
    <Media id="episode-media-redacted-1" container="mkv" duration="3600000" bitrate="4000" width="1280" height="720">
      <Part id="episode-part-redacted" key="/library/parts/episode-part-redacted/file.mkv" file="/redacted/upstream/path/episode.mkv" size="2048" container="mkv" />
    </Media>
  </Video>
</MediaContainer>
"#;
