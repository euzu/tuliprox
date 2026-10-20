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

pub const EMBY_MOVIES_PAGE_JSON: &str = r#"
{
  "Items": [
    {
      "Id": "movie-redacted-1",
      "Name": "Movie Redacted",
      "OriginalTitle": "Movie Redacted Original",
      "SortName": "movie redacted",
      "Type": "Movie",
      "ProductionYear": 2024,
      "Overview": "Redacted overview text.",
      "Taglines": ["Redacted tagline"],
      "Genres": ["Action", "Drama"],
      "Studios": [{ "Name": "Studio Redacted", "Id": "studio-redacted-1" }],
      "People": [
        { "Name": "Actor Redacted", "Id": "person-redacted-1", "Role": "Role Redacted", "Type": "Actor" },
        { "Name": "Director Redacted", "Id": "person-redacted-2", "Type": "Director" }
      ],
      "OfficialRating": "PG-13",
      "CommunityRating": 7.4,
      "PremiereDate": "2024-03-01T00:00:00.0000000Z",
      "DateCreated": "2024-04-02T10:15:30.1234567Z",
      "ProductionLocations": ["Redacted Country"],
      "RunTimeTicks": 72000000000,
      "LocationType": "FileSystem",
      "ParentId": "library-redacted-movies",
      "ProviderIds": { "Tmdb": "12345", "Imdb": "tt0000001" },
      "ImageTags": { "Primary": "image-tag-redacted", "Logo": "logo-tag-redacted" },
      "BackdropImageTags": ["backdrop-tag-redacted"],
      "Path": "/redacted/upstream/path/movie.mkv",
      "UserData": { "Played": false, "PlaybackPositionTicks": 0 },
      "MediaSources": [
        {
          "Id": "media-source-redacted-1",
          "Protocol": "File",
          "IsRemote": false,
          "Container": "mkv",
          "Path": "/redacted/upstream/path/movie.mkv",
          "Bitrate": 8000000,
          "RunTimeTicks": 72000000000,
          "SupportsDirectPlay": true,
          "SupportsDirectStream": true,
          "MediaStreams": [
            { "Type": "Video", "Codec": "h264", "Width": 1920, "Height": 1080, "Index": 0 },
            { "Type": "Audio", "Codec": "aac", "Channels": 6, "Index": 1 },
            { "Type": "Subtitle", "Codec": "srt", "Index": 2 }
          ]
        }
      ]
    },
    {
      "Id": "movie-redacted-2",
      "Name": "Minimal Movie Redacted",
      "Type": "Movie"
    }
  ],
  "TotalRecordCount": 2,
  "StartIndex": 0
}
"#;

pub const EMBY_SERIES_PAGE_JSON: &str = r#"
{
  "Items": [
    {
      "Id": "series-redacted-1",
      "Name": "Series Redacted",
      "OriginalTitle": "Series Redacted Original",
      "SortName": "series redacted",
      "Type": "Series",
      "ProductionYear": 2020,
      "Overview": "Redacted series overview.",
      "Genres": ["Drama"],
      "Studios": [{ "Name": "Network Redacted", "Id": "studio-redacted-2" }],
      "People": [{ "Name": "Actor Redacted", "Id": "person-redacted-1", "Type": "Actor" }],
      "OfficialRating": "TV-14",
      "CommunityRating": 8.1,
      "PremiereDate": "2020-01-10T00:00:00.0000000Z",
      "DateCreated": "2024-04-02T10:15:30.1234567Z",
      "ChildCount": 2,
      "RecursiveItemCount": 14,
      "LocationType": "FileSystem",
      "ParentId": "library-redacted-tv",
      "ProviderIds": { "Tvdb": "67890", "Tmdb": "54321" },
      "ImageTags": { "Primary": "image-tag-redacted" },
      "Path": "/redacted/upstream/path/series"
    }
  ],
  "TotalRecordCount": 1,
  "StartIndex": 0
}
"#;

pub const EMBY_SEASONS_PAGE_JSON: &str = r#"
{
  "Items": [
    {
      "Id": "season-redacted-1",
      "Name": "Season 1",
      "Type": "Season",
      "IndexNumber": 1,
      "SeriesId": "series-redacted-1",
      "SeriesName": "Series Redacted",
      "ParentId": "series-redacted-1",
      "ChildCount": 8,
      "RecursiveItemCount": 8,
      "PremiereDate": "2020-01-10T00:00:00.0000000Z",
      "LocationType": "FileSystem",
      "ImageTags": { "Primary": "image-tag-redacted" }
    },
    {
      "Id": "season-redacted-2",
      "Name": "Season 2",
      "Type": "Season",
      "IndexNumber": 2,
      "SeriesId": "series-redacted-1",
      "SeriesName": "Series Redacted",
      "ParentId": "series-redacted-1",
      "ChildCount": 6,
      "RecursiveItemCount": 6,
      "LocationType": "FileSystem"
    }
  ],
  "TotalRecordCount": 2,
  "StartIndex": 0
}
"#;

pub const EMBY_EPISODES_PAGE_JSON: &str = r#"
{
  "Items": [
    {
      "Id": "episode-redacted-1",
      "Name": "Episode Redacted One",
      "Type": "Episode",
      "IndexNumber": 1,
      "ParentIndexNumber": 1,
      "SeriesId": "series-redacted-1",
      "SeriesName": "Series Redacted",
      "SeasonId": "season-redacted-1",
      "ParentId": "season-redacted-1",
      "Overview": "Redacted episode overview.",
      "PremiereDate": "2020-01-10T00:00:00.0000000Z",
      "DateCreated": "2024-04-02T10:15:30.1234567Z",
      "RunTimeTicks": 27000000000,
      "LocationType": "FileSystem",
      "ProviderIds": { "Tvdb": "11111" },
      "ImageTags": { "Primary": "image-tag-redacted" },
      "Path": "/redacted/upstream/path/series/s01e01.mkv",
      "UserData": { "Played": true, "PlaybackPositionTicks": 0 },
      "MediaSources": [
        {
          "Id": "media-source-redacted-2",
          "Protocol": "File",
          "IsRemote": false,
          "Container": "mkv",
          "Path": "/redacted/upstream/path/series/s01e01.mkv",
          "Bitrate": 4000000,
          "RunTimeTicks": 27000000000,
          "SupportsDirectPlay": true,
          "SupportsDirectStream": true,
          "MediaStreams": [
            { "Type": "Video", "Codec": "hevc", "Width": 1280, "Height": 720, "Index": 0 },
            { "Type": "Audio", "Codec": "ac3", "Channels": 2, "Index": 1 }
          ]
        }
      ]
    },
    {
      "Id": "episode-redacted-2",
      "Name": "Episode Redacted Two (missing)",
      "Type": "Episode",
      "IndexNumber": 2,
      "ParentIndexNumber": 1,
      "SeriesId": "series-redacted-1",
      "SeriesName": "Series Redacted",
      "SeasonId": "season-redacted-1",
      "ParentId": "season-redacted-1",
      "PremiereDate": "2020-01-17T00:00:00.0000000Z",
      "LocationType": "Virtual"
    },
    {
      "Id": "episode-redacted-3",
      "Name": "Episode Redacted Three (remote)",
      "Type": "Episode",
      "IndexNumber": 3,
      "ParentIndexNumber": 1,
      "SeriesId": "series-redacted-1",
      "SeriesName": "Series Redacted",
      "SeasonId": "season-redacted-1",
      "ParentId": "season-redacted-1",
      "RunTimeTicks": 27000000000,
      "LocationType": "FileSystem",
      "Path": "/redacted/upstream/path/series/s01e03.strm",
      "MediaSources": [
        {
          "Id": "media-source-redacted-3",
          "Protocol": "Http",
          "IsRemote": true,
          "Container": "strm",
          "Path": "http://redacted.invalid/stream/redacted.ts",
          "SupportsDirectPlay": true,
          "SupportsDirectStream": true,
          "MediaStreams": [
            { "Type": "Video", "Codec": "h264", "Width": 1920, "Height": 1080, "Index": 0 }
          ]
        }
      ]
    }
  ],
  "TotalRecordCount": 3,
  "StartIndex": 0
}
"#;

pub const EMBY_VIRTUAL_FOLDERS_JSON: &str = r#"
[
  {
    "Name": "Movies",
    "Locations": ["/redacted/media/movies"],
    "CollectionType": "movies",
    "ItemId": "library-redacted-movies",
    "Id": "library-redacted-movies-guid"
  },
  {
    "Name": "TV",
    "Locations": ["/redacted/media/tv"],
    "CollectionType": "tvshows",
    "ItemId": "library-redacted-tv",
    "Id": "library-redacted-tv-guid"
  },
  {
    "Name": "Mixed",
    "Locations": ["/redacted/media/mixed"],
    "ItemId": "library-redacted-mixed",
    "Id": "library-redacted-mixed-guid"
  },
  {
    "Name": "Collections",
    "Locations": [],
    "CollectionType": "boxsets",
    "ItemId": "library-redacted-boxsets",
    "Id": "library-redacted-boxsets-guid"
  }
]
"#;

pub const EMBY_PUBLIC_SYSTEM_INFO_JSON: &str = r#"
{
  "LocalAddress": "http://192.0.2.10:8096",
  "ServerName": "Server Redacted",
  "Version": "4.8.0.0",
  "ProductName": "Emby Server",
  "OperatingSystem": "Linux",
  "Id": "server-redacted-id",
  "StartupWizardCompleted": true
}
"#;

pub const EMBY_AUTH_RESULT_JSON: &str = r#"
{
  "User": {
    "Name": "user-redacted",
    "ServerId": "server-redacted-id",
    "Id": "user-redacted-id",
    "HasPassword": true
  },
  "SessionInfo": {
    "Id": "session-redacted",
    "UserId": "user-redacted-id",
    "DeviceName": "Tuliprox"
  },
  "AccessToken": "token-redacted",
  "ServerId": "server-redacted-id"
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
