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
  <Video ratingKey="rating-redacted-1" key="/library/metadata/rating-redacted-1" type="movie" title="Movie Redacted" titleSort="Movie Redacted" originalTitle="Original Movie Redacted" year="2024" originallyAvailableAt="2024-01-02" summary="Movie summary redacted" tagline="Tagline redacted" studio="Studio Redacted" contentRating="PG-13" contentRatingAge="13" audienceRating="8.2" thumb="/library/metadata/rating-redacted-1/thumb" art="/library/metadata/rating-redacted-1/art" addedAt="1700000000" updatedAt="1700000001">
    <Guid id="tmdb://12345" />
    <Guid id="imdb://tt-redacted" />
    <Genre tag="Drama" />
    <Country tag="Country Redacted" />
    <Director tag="Director Redacted" />
    <Writer tag="Writer Redacted" />
    <Role tag="Actor Redacted" />
    <Image type="coverPoster" url="/library/metadata/rating-redacted-1/thumb" />
    <Media id="media-redacted-1" container="mkv" duration="7200000" bitrate="8000" width="1920" height="1080" videoCodec="hevc" audioCodec="eac3" audioChannels="6" videoResolution="1080">
      <Part id="part-redacted" key="/library/parts/part-redacted/file.mkv" file="/redacted/upstream/path/movie.mkv" size="1024" container="mkv" />
    </Media>
  </Video>
</MediaContainer>
"#;

pub const PLEX_SHOWS_XML: &str = r#"
<MediaContainer size="1" totalSize="1">
  <Directory ratingKey="series-redacted-1" key="/library/metadata/series-redacted-1/children" type="show" title="Show Redacted" titleSort="Show Redacted" originalTitle="Original Show Redacted" year="2024" originallyAvailableAt="2024-01-01" summary="Show summary redacted" tagline="Show tagline redacted" studio="Network Redacted" contentRating="TV-14" contentRatingAge="14" audienceRating="7.8" thumb="/library/metadata/series-redacted-1/thumb" art="/library/metadata/series-redacted-1/art" theme="/library/metadata/series-redacted-1/theme" childCount="1" leafCount="2" viewedLeafCount="0" addedAt="1700000100" updatedAt="1700000101">
    <Guid id="tmdb://222" />
    <Guid id="tvdb://333" />
    <Genre tag="Mystery" />
    <Country tag="Country Redacted" />
    <Role tag="Actor Redacted" />
    <Image type="coverPoster" url="/library/metadata/series-redacted-1/thumb" />
  </Directory>
</MediaContainer>
"#;

pub const PLEX_SEASONS_XML: &str = r#"
<MediaContainer size="1" totalSize="1">
  <Directory ratingKey="season-redacted-1" key="/library/metadata/season-redacted-1/children" type="season" title="Season 1" index="1" parentRatingKey="series-redacted-1" parentGuid="tmdb://222" parentTitle="Show Redacted" parentIndex="1" year="2024" summary="Season summary redacted" thumb="/library/metadata/season-redacted-1/thumb" art="/library/metadata/season-redacted-1/art" leafCount="2" viewedLeafCount="0" addedAt="1700000200" updatedAt="1700000201">
    <Guid id="tvdb://333" />
    <Image type="coverPoster" url="/library/metadata/season-redacted-1/thumb" />
  </Directory>
</MediaContainer>
"#;

pub const PLEX_EPISODES_XML: &str = r#"
<MediaContainer size="1" totalSize="1">
  <Video ratingKey="episode-redacted-1" key="/library/metadata/episode-redacted-1" type="episode" title="Episode Redacted" originallyAvailableAt="2024-02-03" summary="Episode summary redacted" guid="tmdb://67890" grandparentRatingKey="series-redacted-1" grandparentGuid="tmdb://222" grandparentTitle="Show Redacted" parentRatingKey="season-redacted-1" parentGuid="tvdb://333" parentTitle="Season 1" parentIndex="1" index="2" thumb="/library/metadata/episode-redacted-1/thumb" addedAt="1700000300" updatedAt="1700000301">
    <Media id="episode-media-redacted-1" container="mkv" duration="3600000" bitrate="4500" width="1280" height="720" videoCodec="h264" audioCodec="aac" audioChannels="2" videoResolution="720">
      <Part id="episode-part-redacted" key="/library/parts/episode-part-redacted/file.mkv" file="/redacted/upstream/path/episode.mkv" size="2048" container="mkv" />
    </Media>
  </Video>
</MediaContainer>
"#;
