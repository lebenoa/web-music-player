mod utils;

use audiotags::{MimeType, Picture};
use axum::{
    extract::{Multipart, Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    sync::Arc,
};
use tokio::{process::Command, sync::Mutex};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use ytmapi_rs::{auth::BrowserToken, common::YoutubeID};

const MUSIC_DIR: &str = "music";
const IMG_DIR: &str = "img";
const TEMP_DIR: &str = "temp";
const PUBLIC_DIR: &str = "public";

#[cfg_attr(debug_assertions, derive(Debug))]
#[derive(Serialize, Deserialize, Clone)]
pub struct QueueItem {
    filename: String,
    title: String,
    artist: String,
    artists: Option<Vec<String>>,
    thumbnail: Option<String>,
    duration: Option<u64>,
    artist_thumbnail: Option<String>,
    url: String,
}

#[cfg_attr(debug_assertions, derive(Debug))]
#[derive(Clone, Serialize, Deserialize)]
pub struct PlaylistSession {
    #[serde(skip)]
    pub is_empty: bool,

    pub current_time: f32,
    pub current_index: u32,
    pub queue: Vec<QueueItem>,
}

impl Default for PlaylistSession {
    fn default() -> Self {
        Self {
            is_empty: true,
            current_time: 0.0,
            current_index: 0,
            queue: vec![],
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    youtube_search: Arc<rusty_ytdl::search::YouTube>,
    youtube_music_search: Arc<ytmapi_rs::YtMusic<BrowserToken>>,
    mp3_reader: Arc<audiotags::Tag>,
    mp4_reader: Arc<audiotags::Tag>,
    recently_played: Arc<Mutex<VecDeque<Track>>>,
    playlist_session: Arc<Mutex<PlaylistSession>>,
}

#[tokio::main]
async fn main() {
    #[cfg(debug_assertions)]
    let level = tracing::Level::DEBUG;

    #[cfg(not(debug_assertions))]
    let level = tracing::Level::INFO;

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_line_number(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    if let Err(e) = Command::new("yt-dlp")
        .args(["-U"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::null())
        .spawn()
    {
        tracing::warn!("Cannot check for yt-dlp update: {}", e);
    }

    let state = AppState {
        youtube_search: Arc::new(rusty_ytdl::search::YouTube::new().unwrap()),
        youtube_music_search: Arc::new(
            ytmapi_rs::YtMusic::from_cookie_file("id.txt")
                .await
                .expect("Init YtMusic Instance"),
        ),
        mp3_reader: Arc::new(audiotags::Tag::new().with_tag_type(audiotags::TagType::Id3v2)),
        mp4_reader: Arc::new(audiotags::Tag::new().with_tag_type(audiotags::TagType::Mp4)),
        recently_played: Arc::new(Mutex::new(VecDeque::with_capacity(10))),
        playlist_session: Arc::new(Mutex::new(PlaylistSession::default())),
    };

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));

        loop {
            interval.tick().await;
            _ = std::fs::remove_dir_all(TEMP_DIR);
            _ = std::fs::create_dir(TEMP_DIR);
        }
    });

    _ = std::fs::create_dir(MUSIC_DIR);
    _ = std::fs::create_dir(IMG_DIR);
    _ = std::fs::create_dir(PUBLIC_DIR);

    let entries = std::fs::read_dir(MUSIC_DIR).map_err(|e| e.to_string());

    if let Ok(entries) = entries {
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("{}", e);
                    continue;
                }
            };

            let mp3_reader_clone = state.mp3_reader.clone();
            let mp4_reader_clone = state.mp4_reader.clone();
            tokio::spawn(async move {
                let filename = entry.file_name().to_string_lossy().to_string();
                let (title, ext) = {
                    let last_dot = filename.rfind('.');

                    match last_dot {
                        Some(d) => (filename[0..d].to_string(), filename[d + 1..].to_string()),
                        None => (filename.clone(), "mp3".to_string()),
                    }
                };

                let reader = match ext.as_str() {
                    "mp3" => mp3_reader_clone,
                    "mp4" | "m4a" => mp4_reader_clone,
                    _ => {
                        tracing::error!("Unrecognize format ({})", filename);
                        return;
                    }
                };

                match reader.read_from_path(format!("{}/{}", MUSIC_DIR, filename)) {
                    Ok(mut tag) => {
                        let cover = tag.album_cover();
                        if let Some(c) = cover {
                            let path = format!("img/{}.jpeg", title);
                            match c.mime_type {
                                MimeType::Jpeg => {
                                    std::fs::write(path, c.data).unwrap();
                                }
                                _ => {
                                    tracing::info!("Converting image for: {}...", filename);

                                    let img = image::load_from_memory_with_format(
                                        c.data,
                                        match c.mime_type {
                                            MimeType::Jpeg => unreachable!("Should not be jpeg"),
                                            MimeType::Png => image::ImageFormat::Png,
                                            MimeType::Bmp => image::ImageFormat::Bmp,
                                            MimeType::Gif => image::ImageFormat::Gif,
                                            MimeType::Tiff => image::ImageFormat::Tiff,
                                        },
                                    )
                                    .unwrap()
                                    .into_rgb8();

                                    let mut buffer = Vec::with_capacity(img.len());
                                    img.write_to(
                                        &mut std::io::Cursor::new(&mut buffer),
                                        image::ImageFormat::Jpeg,
                                    )
                                    .unwrap();

                                    tag.set_album_cover(Picture::new(&buffer, MimeType::Jpeg));
                                    tag.write_to_path(&format!("{}/{}", MUSIC_DIR, filename))
                                        .unwrap();
                                    std::fs::write(path, buffer).unwrap();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("{} ({})", e, filename);
                    }
                };
            });
        }
    }

    let api = Router::new()
        .route("/files", get(list_file))
        .route("/search", post(search_api))
        .route("/msearch", post(search_music_api))
        .route("/crop", post(crop_api))
        .route("/edit", post(edit_api))
        .route("/delete", post(delete_api))
        .route("/artist-playlist", get(group_by_artist));

    let app = Router::new()
        .route("/", get(index))
        .route("/download", post(download_file))
        .route("/temp-download/:id", get(temp_download))
        .route("/history", post(add_to_history))
        .route("/save-playlist", post(save_playlist))
        .route("/load-playlist", get(load_playlist))
        .route("/clear-playlist", post(clear_playlist))
        .nest("/api", api)
        .with_state(state)
        .nest_service("/m", ServeDir::new(MUSIC_DIR))
        .nest_service("/td", ServeDir::new(TEMP_DIR))
        .nest_service("/img", ServeDir::new(IMG_DIR))
        .fallback_service(ServeDir::new(PUBLIC_DIR))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:1809").await.unwrap();
    tracing::info!("Listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Track {
    filename: String,
    title: String,
    artist: String,
    artists: Option<Vec<String>>,
    thumbnail: Option<String>,
    duration: Option<u64>,
    artist_thumbnail: Option<String>,
}

impl PartialEq for Track {
    fn eq(&self, other: &Self) -> bool {
        self.filename == other.filename
    }
}

async fn add_to_history(
    State(state): State<AppState>,
    Json(track): Json<Track>,
) -> impl IntoResponse {
    tracing::debug!("Adding to history: {}", track.filename);

    let mut recently_played = state.recently_played.lock().await;

    if recently_played.contains(&track) {
        let pos = recently_played.iter().position(|x| *x == track).unwrap();
        recently_played.remove(pos);
        recently_played.push_front(track);

        return (StatusCode::OK, Json(recently_played.clone())).into_response();
    }

    if recently_played.len() >= 10 {
        recently_played.pop_back();
    }
    recently_played.push_front(track);

    (StatusCode::OK, Json(recently_played.clone())).into_response()
}

#[derive(Serialize)]
struct FileApiResponse {
    recently_played: VecDeque<Track>,
    files: Vec<Track>,
}

async fn list_file(State(state): State<AppState>) -> Result<Json<FileApiResponse>, String> {
    let entries = std::fs::read_dir(MUSIC_DIR).map_err(|e| e.to_string())?;
    let mut files = vec![];

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                return Err(e.to_string());
            }
        };

        let filename = entry.file_name().to_string_lossy().to_string();
        let (title, ext) = {
            let last_dot = filename.rfind('.');

            match last_dot {
                Some(d) => (filename[0..d].to_string(), filename[d + 1..].to_string()),
                None => (filename.clone(), "mp3".to_string()),
            }
        };

        let reader = match ext.as_str() {
            "mp3" => state.mp3_reader.clone(),
            "mp4" | "m4a" => state.mp4_reader.clone(),
            _ => {
                tracing::error!("Unrecognize format: {}", filename);
                continue;
            }
        };
        let image = format!("/img/{}.jpeg", title);

        let artist = match reader.read_from_path(format!("{}/{}", MUSIC_DIR, filename)) {
            Ok(mut tag) => {
                if !std::path::Path::new(&image[1..]).exists() {
                    let cover = tag.album_cover();
                    if let Some(c) = cover {
                        match c.mime_type {
                            MimeType::Jpeg => {
                                if let Err(e) = std::fs::write(&image[1..], c.data) {
                                    tracing::error!("Failed to save image ({}): {}", filename, e);
                                }
                            }
                            _ => {
                                tracing::info!("Converting image for: {}...", filename);

                                let img = image::load_from_memory_with_format(
                                    c.data,
                                    match c.mime_type {
                                        MimeType::Jpeg => unreachable!("Should not be jpeg"),
                                        MimeType::Png => image::ImageFormat::Png,
                                        MimeType::Bmp => image::ImageFormat::Bmp,
                                        MimeType::Gif => image::ImageFormat::Gif,
                                        MimeType::Tiff => image::ImageFormat::Tiff,
                                    },
                                )
                                .unwrap()
                                .into_rgb8();

                                let mut buffer = Vec::with_capacity(img.len());
                                img.write_to(
                                    &mut std::io::Cursor::new(&mut buffer),
                                    image::ImageFormat::Jpeg,
                                )
                                .unwrap();

                                tag.set_album_cover(Picture::new(&buffer, MimeType::Jpeg));
                                tag.write_to_path(&format!("{}/{}", MUSIC_DIR, filename))
                                    .unwrap();
                                std::fs::write(&image[1..], buffer).unwrap();
                            }
                        }
                    }
                }

                tag.artist().map(|a| a.to_string())
            }
            Err(e) => {
                tracing::error!("{}\n{}", e, filename);
                None
            }
        };

        files.push(Track {
            filename,
            title,
            artist: artist.unwrap_or_else(|| "Unknown".to_string()),
            artists: None,
            thumbnail: Some(image),
            duration: None,
            artist_thumbnail: None,
        });
    }

    Ok(Json(FileApiResponse {
        recently_played: state.recently_played.lock().await.clone(),
        files,
    }))
}

async fn search_api(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<Vec<Track>>, String> {
    tracing::info!("Searching: {}", body);
    let search_result = state
        .youtube_search
        .search(
            body,
            Some(&rusty_ytdl::search::SearchOptions {
                limit: 20,
                search_type: rusty_ytdl::search::SearchType::Video,
                safe_search: false,
            }),
        )
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|i| match i {
            rusty_ytdl::search::SearchResult::Video(mut x) => Track {
                filename: x.id,
                title: x.title,
                artist: x.channel.name,
                artists: None,
                thumbnail: Some(x.thumbnails.swap_remove(x.thumbnails.len() - 1).url),
                duration: Some(x.duration / 1000),
                artist_thumbnail: Some(x.channel.icon.swap_remove(x.channel.icon.len() - 1).url),
            },
            _ => unreachable!(),
        })
        .collect::<Vec<Track>>();

    Ok(Json(search_result))
}

async fn search_music_api(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<Vec<Track>>, String> {
    tracing::info!("Searching Music `{}`...", body);

    let search_results = state
        .youtube_music_search
        .search_songs(body)
        .await
        .map_err(|e| format!("Search failed: {e}"))?;

    Ok(Json(
        search_results
            .into_iter()
            .map(|sr| {
                let duration = {
                    let mut parts = sr.duration.split(':').collect::<Vec<&str>>();
                    let len = parts.len();
                    match len {
                        2 => {
                            let seconds = parts.remove(1).parse::<u64>().unwrap_or_else(|_| {
                                panic!("Duration is not number: {}", sr.duration)
                            });

                            let minutes = parts.remove(0).parse::<u64>().unwrap_or_else(|_| {
                                panic!("Duration is not number: {}", sr.duration)
                            }) * 60;

                            Some(seconds + minutes)
                        }
                        _ => None,
                    }
                };

                Track {
                    filename: sr.video_id.get_raw().to_string(),
                    title: sr.title,
                    artists: Some(
                        sr.artist
                            .split(&['&', ','])
                            .filter(|p| !p.is_empty())
                            .map(|i| i.trim().to_string())
                            .collect(),
                    ),
                    artist: sr.artist,
                    duration,
                    thumbnail: Some(
                        sr.thumbnails
                            .last()
                            .unwrap()
                            .url
                            .replace("w120-h120", "w300-h300"),
                    ),
                    artist_thumbnail: None,
                }
            })
            .collect(),
    ))
}

#[cfg(debug_assertions)]
async fn index() -> Result<Html<String>, String> {
    let index_content = std::fs::read_to_string("index.html").map_err(|e| e.to_string())?;

    Ok(Html(index_content))
}

#[cfg(not(debug_assertions))]
const INDEX: &str = include_str!("../index.html");

#[cfg(not(debug_assertions))]
async fn index() -> Result<Html<&'static str>, String> {
    Ok(Html(INDEX))
}

async fn save_playlist(
    State(state): State<AppState>,
    Json(session): Json<PlaylistSession>,
) -> impl IntoResponse {
    let mut prev_session = state.playlist_session.lock().await;
    *prev_session = session;
    prev_session.is_empty = false;

    (StatusCode::OK, "success")
}

async fn load_playlist(State(state): State<AppState>) -> impl IntoResponse {
    let session = state.playlist_session.lock().await;
    if session.is_empty {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "No session stored".to_string(),
        );
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&*session).expect("serialize session to json"),
    )
}

async fn clear_playlist(State(state): State<AppState>) -> impl IntoResponse {
    let mut session = state.playlist_session.lock().await;
    if session.is_empty {
        return (StatusCode::OK, "Ok");
    }

    *session = PlaylistSession::default();

    (StatusCode::OK, "Ok")
}

//#[derive(Serialize)]
//struct AutoPlaylist {
//    name: String,
//    items: Vec<Track>,
//}

async fn group_by_artist(
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, Vec<Track>>>, String> {
    let entries = std::fs::read_dir(MUSIC_DIR).map_err(|e| e.to_string())?;
    let mut map: HashMap<String, Vec<Track>> = HashMap::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                return Err(e.to_string());
            }
        };

        let filename = entry.file_name().to_string_lossy().to_string();
        let (title, ext) = {
            let last_dot = filename.rfind('.');

            match last_dot {
                Some(d) => (filename[0..d].to_string(), filename[d + 1..].to_string()),
                None => (filename.clone(), "mp3".to_string()),
            }
        };

        let reader = match ext.as_str() {
            "mp3" => state.mp3_reader.clone(),
            "mp4" | "m4a" => state.mp4_reader.clone(),
            _ => {
                tracing::error!("Unrecognize format: {}", filename);
                continue;
            }
        };
        let image = format!("/img/{}.jpeg", title);

        let artist = match reader.read_from_path(format!("{}/{}", MUSIC_DIR, filename)) {
            Ok(mut tag) => {
                if !std::path::Path::new(&image[1..]).exists() {
                    let cover = tag.album_cover();
                    if let Some(c) = cover {
                        match c.mime_type {
                            MimeType::Jpeg => {
                                if let Err(e) = std::fs::write(&image[1..], c.data) {
                                    tracing::error!("Failed to save image ({}): {}", filename, e);
                                }
                            }
                            _ => {
                                tracing::info!("Converting image for: {}...", filename);

                                let img = image::load_from_memory_with_format(
                                    c.data,
                                    match c.mime_type {
                                        MimeType::Jpeg => unreachable!("Should not be jpeg"),
                                        MimeType::Png => image::ImageFormat::Png,
                                        MimeType::Bmp => image::ImageFormat::Bmp,
                                        MimeType::Gif => image::ImageFormat::Gif,
                                        MimeType::Tiff => image::ImageFormat::Tiff,
                                    },
                                )
                                .unwrap()
                                .into_rgb8();

                                let mut buffer = Vec::with_capacity(img.len());
                                img.write_to(
                                    &mut std::io::Cursor::new(&mut buffer),
                                    image::ImageFormat::Jpeg,
                                )
                                .unwrap();

                                tag.set_album_cover(Picture::new(&buffer, MimeType::Jpeg));
                                tag.write_to_path(&format!("{}/{}", MUSIC_DIR, filename))
                                    .unwrap();
                                std::fs::write(&image[1..], buffer).unwrap();
                            }
                        }
                    }
                }

                tag.artist().unwrap_or("Unknown").to_string()
            }
            Err(e) => {
                tracing::error!("{}\n{}", e, filename);
                continue;
            }
        };

        let alone_artist = artist.split(", ").next().unwrap().to_string();
        map.entry(alone_artist)
            .and_modify(|e| {
                e.push(Track {
                    filename: filename.clone(),
                    title: title.clone(),
                    artist: artist.clone(),
                    artists: None,
                    thumbnail: Some(image.clone()),
                    duration: None,
                    artist_thumbnail: None,
                })
            })
            .or_insert_with(|| {
                vec![Track {
                    filename,
                    title,
                    artist,
                    artists: None,
                    thumbnail: Some(image),
                    duration: None,
                    artist_thumbnail: None,
                }]
            });
    }

    map.retain(|_, v| v.len() > 1);

    Ok(Json(map))
}

#[derive(Serialize, Deserialize)]
struct Artist {
    artist: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
}

impl Artist {
    fn get(self) -> String {
        self.artist
            .or(self.uploader)
            .or(self.channel)
            .unwrap_or_else(|| "Unknown".to_string())
    }
}

#[derive(Deserialize)]
struct DownloadResponse {
    title: String,
    description: Option<String>,

    #[serde(flatten)]
    artist: Artist,

    thumbnail: String,
    duration: f32,
}

const MAX_RETRIES: u8 = 3;

async fn download_file(State(state): State<AppState>, body: String) -> impl IntoResponse {
    tracing::info!("Downloading: {}", body);

    let mut i = 0;
    let stdout = loop {
        i += 1;

        let proc = Command::new("yt-dlp")
            .args([
                "-f",
                "bestaudio/best",
                "--no-playlist",
                "--no-warning",
                "--embed-thumbnail",
                "--embed-metadata",
                "--print-json",
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                &format!("{MUSIC_DIR}/%(title)s.%(ext)s"),
                "--",
                &body,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;

        let proc = match proc {
            Ok(proc) => proc,
            Err(e) => {
                let message = format!("Failed to spawn and capture output: {}\n{}", e, e);
                tracing::error!("{}", message);
                if i == MAX_RETRIES {
                    return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
                } else {
                    continue;
                }
            }
        };

        if !proc.stderr.is_empty() {
            let message = unsafe { String::from_utf8_unchecked(proc.stderr) };
            tracing::error!("Yt-DLP stderr: {}", message);
            if i == MAX_RETRIES {
                return (StatusCode::BAD_REQUEST, message).into_response();
            } else {
                continue;
            }
        }

        break proc.stdout;
    };

    #[cfg(debug_assertions)]
    tracing::debug!("Parsing JSON from yt-dlp...");

    let parsed: DownloadResponse = match serde_json::from_slice(&stdout) {
        Ok(j) => j,
        Err(e) => {
            let message = format!("Failed to parse JSON: {}\n{}", e, unsafe {
                String::from_utf8_unchecked(stdout)
            });
            tracing::error!("{}", message);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to parse JSON").into_response();
        }
    };

    let mut image_path = parsed.thumbnail;

    if let Some(d) = parsed.description {
        if d.starts_with("Provided to YouTube by") {
            tracing::info!("Cropping image for {}...", parsed.title);
            let music_path = format!("{MUSIC_DIR}/{}.mp3", parsed.title);
            image_path = format!("{IMG_DIR}/{}.jpeg", parsed.title);

            let mut tag = match state.mp3_reader.read_from_path(&music_path) {
                Ok(t) => t,
                Err(e) => {
                    let message = format!("Open music file error: {e}");
                    tracing::error!("{message} | path: {music_path}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
                }
            };

            let cover = tag.album_cover().unwrap();
            let mut img = image::load_from_memory(cover.data).unwrap().into_rgb8();

            let (width, height) = img.dimensions();
            if width != height {
                let offset = utils::find_offset_to_center(width, height);
                let cropped = image::imageops::crop(&mut img, offset, 0, height, height).to_image();

                let mut buffer = Vec::with_capacity(cropped.len());
                if let Err(e) = cropped.write_to(
                    &mut std::io::Cursor::new(&mut buffer),
                    image::ImageFormat::Jpeg,
                ) {
                    let message = format!("Crop image error: {e}");
                    tracing::error!("{}", message);
                    return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
                }

                _ = std::fs::write(&image_path, &buffer);

                tag.set_album_cover(Picture::new(&buffer, MimeType::Jpeg));
                tag.write_to_path(&music_path).unwrap();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "title": parsed.title,
            "artist": parsed.artist.get(),
            "thumbnail": image_path,
            "duration": parsed.duration
        })),
    )
        .into_response()
}

async fn temp_download(Path(id): Path<String>) -> impl IntoResponse {
    tracing::info!("Downloading to temp: {}", id);
    let fp = format!("temp/{id}.mp3");
    let path = std::path::Path::new(&fp);
    if path.exists() {
        return Ok((StatusCode::OK, format!("/td/{id}.mp3")));
    }

    let mut i = 0;
    loop {
        i += 1;

        let proc = tokio::process::Command::new("yt-dlp")
            .args([
                "-f",
                "bestaudio/best",
                "--no-playlist",
                "--no-warning",
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                &fp,
                "--",
                &id,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .output()
            .await;

        match proc {
            Ok(es) => {
                if !es.stderr.is_empty() {
                    let message = unsafe { String::from_utf8_unchecked(es.stderr) };
                    tracing::error!("Temp Download: Yt-dlp error: {}", message);
                    if i == MAX_RETRIES {
                        return Err((StatusCode::INTERNAL_SERVER_ERROR, message));
                    } else {
                        continue;
                    }
                }
            }
            Err(e) => {
                let message = format!("Error spawn and capture proc: {e}");
                tracing::error!("{}", message);
                if i == MAX_RETRIES {
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, message));
                } else {
                    continue;
                }
            }
        }

        return Ok((StatusCode::OK, format!("/td/{id}.mp3")));
    }
}

async fn edit_api(State(state): State<AppState>, mut multipart: Multipart) -> impl IntoResponse {
    let mut filename = String::new();
    let mut title = String::new();
    let mut path = String::new();
    let mut tag = None;

    let mut matched_title = true;

    while let Some(field) = multipart.next_field().await.unwrap() {
        match field.name().unwrap() {
            "filename" => {
                filename = field.text().await.unwrap();
                path = format!("{MUSIC_DIR}/{filename}");

                match state.mp3_reader.read_from_path(&path) {
                    Ok(t) => tag = Some(t),
                    Err(e) => {
                        return (StatusCode::BAD_REQUEST, format!("Failed to read tag: {e}"))
                            .into_response();
                    }
                }
            }
            "title" => {
                title = field.text().await.unwrap();

                if title == utils::without_extension(&filename) {
                    continue;
                }

                matched_title = false;
                tag.as_mut().unwrap().set_title(&title);
            }
            "artist" => {
                let artist = field.text().await.unwrap();

                tag.as_mut().unwrap().set_artist(&artist);
            }
            "thumbnail" => {
                let content_type = field.content_type().map(|c| c.to_string());
                let thumbnail = field.bytes().await.unwrap();
                if thumbnail.is_empty() {
                    continue;
                }

                match content_type {
                    Some(content_type) => {
                        if !content_type.starts_with("image/") {
                            return (StatusCode::BAD_REQUEST, "Invalid content type")
                                .into_response();
                        }

                        let mime = MimeType::try_from(content_type.as_str());
                        if mime.is_err() {
                            return (StatusCode::BAD_REQUEST, "Invalid content type")
                                .into_response();
                        }

                        tag.as_mut()
                            .unwrap()
                            .set_album_cover(Picture::new(thumbnail.as_ref(), mime.unwrap()));

                        std::fs::write(
                            format!(
                                "{IMG_DIR}/{title}.{}",
                                content_type.strip_prefix("image/").unwrap()
                            ),
                            thumbnail,
                        )
                        .unwrap();
                    }
                    None => {
                        tracing::warn!("Image was uploaded but has no content type");
                        continue;
                    }
                }
            }
            _ => continue,
        }
    }

    tag.as_mut().unwrap().write_to_path(&path).unwrap();

    if !matched_title {
        let new_filename = format!(
            "{MUSIC_DIR}/{title}{}",
            &filename[filename.rfind('.').unwrap()..]
        );
        tracing::debug!("Renaming {path} to {new_filename}");
        std::fs::rename(path, new_filename).unwrap();
    }

    (StatusCode::OK, "OK").into_response()
}

async fn delete_api(State(state): State<AppState>, body: String) -> impl IntoResponse {
    let path = format!("{MUSIC_DIR}/{body}");

    let tag = state.mp3_reader.read_from_path(&path).unwrap();
    let cover = tag.album_cover();

    if let Some(c) = cover {
        if let Err(e) = std::fs::remove_file(format!(
            "{IMG_DIR}/{}.{}",
            utils::without_extension(&body),
            String::from(c.mime_type).strip_prefix("image/").unwrap()
        )) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    if let Err(e) = std::fs::remove_file(path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    (StatusCode::OK, "OK").into_response()
}

#[derive(Deserialize)]
struct CropRequest {
    filename: String,
    image: String,
}

async fn crop_api(
    State(state): State<AppState>,
    Json(body): Json<CropRequest>,
) -> impl IntoResponse {
    let music_path = format!("{MUSIC_DIR}/{}", body.filename);
    let image_path = if let Some(i) = body.image.rfind('?') {
        body.image[..i].strip_prefix('/').unwrap()
    } else {
        body.image.strip_prefix('/').unwrap()
    };
    let mut img = match image::open(image_path) {
        Ok(img) => img.into_rgb8(),
        Err(e) => {
            let message = format!("Open image error: {e}");
            tracing::error!("{message} | path: {image_path}");
            return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
        }
    };

    let (width, height) = img.dimensions();
    if width == height {
        return (StatusCode::BAD_REQUEST, "Already square").into_response();
    }

    let offset = utils::find_offset_to_center(width, height);
    let cropped = image::imageops::crop(&mut img, offset, 0, height, height).to_image();

    let mut buffer = Vec::with_capacity(img.len());
    if let Err(e) = cropped.write_to(
        &mut std::io::Cursor::new(&mut buffer),
        image::ImageFormat::Jpeg,
    ) {
        let message = format!("Crop image error: {e}");
        tracing::error!("{}", message);
        return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
    }

    _ = std::fs::write(
        format!("{}.jpeg", utils::without_extension(image_path)),
        &buffer,
    );

    let mut tag = state.mp3_reader.read_from_path(&music_path).unwrap();
    tag.set_album_cover(Picture::new(&buffer, MimeType::Jpeg));
    tag.write_to_path(&music_path).unwrap();

    (StatusCode::OK, "OK").into_response()
}
