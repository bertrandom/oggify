extern crate env_logger;
extern crate librespot_audio;
extern crate librespot_core;
extern crate librespot_metadata;
#[macro_use]
extern crate log;
extern crate regex;
extern crate scoped_threadpool;
extern crate tokio;

use std::io::Read;
use std::process::Command;

use env_logger::{Builder, Env};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::authentication::Credentials;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_id::SpotifyId;
use librespot_core::{SpotifyUri};

use librespot_core::cache::Cache;
use librespot_core::Error;


use librespot_metadata::{Metadata, Track};
use librespot_metadata::audio::{AudioFileFormat};
use regex::Regex;

// Read and write vorbiscomment metadata
use oggvorbismeta::{
    replace_comment_header, CommentHeader, VorbisComments,
};
use std::fs::File;
use std::io::Cursor;

use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {

    #[clap(flatten)]
    group: Group,

    /// Optional name to operate on
    name: Option<String>,

}

#[derive(Debug, clap::Args)]
#[group(required = true, multiple = false)]
pub struct Group {
    /// Argument1.
    #[clap(short, long)]
    url: Option<String>,

    /// Input file with URLs, use '-' for stdin
    #[clap(long)]
    urls: Option<String>,

}

const CACHE: &str = ".cache";
const CACHE_FILES: &str = ".cache/files";
const OUTPUT_DIR: &str = "output";

async fn process_track(
    spotify_uri: &str,
    session: &Session,
) -> std::result::Result<(), Box<dyn std::error::Error>> {

    let uri = SpotifyUri::from_uri(spotify_uri)?;
    let id_str = uri.to_id().unwrap();
    let id = SpotifyId::from_base62(&id_str)?;

    let track = Track::get(&session, &uri).await?;

    let artists = track
        .artists
        .iter()
        .map(|a| a.name.clone())
        .collect::<Vec<_>>();

    let track_name = track.name.clone();
    let track_id = track.id.to_base62().unwrap();

    let file_id = track
        .files
        .get(&AudioFileFormat::OGG_VORBIS_320)
        .or(track.files.get(&AudioFileFormat::OGG_VORBIS_160))
        .or(track.files.get(&AudioFileFormat::OGG_VORBIS_96))
        .ok_or(Error::unavailable("No OGG Vorbis format"))?;

    let key = session.audio_key().request(id, *file_id).await.ok();

    let mut encrypted_file = AudioFile::open(&session, *file_id, 320).await?;
    let mut buffer = Vec::new();
    encrypted_file.read_to_end(&mut buffer)?;

    let mut decrypted_buffer = Vec::new();
    AudioDecrypt::new(key, &buffer[..]).read_to_end(&mut decrypted_buffer)?;

    let ogg_path = format!("{}/{}.ogg", OUTPUT_DIR, track_id);
    std::fs::write(&ogg_path, &decrypted_buffer[0xa7..])?;

    // --- tagging ---
    let mut f_in_ram = Vec::new();
    File::open(&ogg_path)?.read_to_end(&mut f_in_ram)?;

    let mut comments = CommentHeader::new();
    comments.set_vendor("Ogg");

    for artist in &artists {
        comments.add_tag_single("artist", artist);
    }

    comments.add_tag_single("album", track.album.name.clone());
    comments.add_tag_single("tracknumber", track.number.to_string());
    comments.add_tag_single("title", track_name.clone());
    comments.add_tag_single("date", track.album.date.year().to_string());

    let tagged_ogg = format!("{}/{}-tagged.ogg", OUTPUT_DIR, track_id);
    let mut tagged = replace_comment_header(Cursor::new(f_in_ram), &comments)?;

    info!("Save to disk");
    let mut f_out_disk = File::create(tagged_ogg.clone()).unwrap();
    std::io::copy(&mut tagged, &mut f_out_disk).unwrap();

    // --- ffmpeg ---
    let output_mp3 = format!(
        "{}/{} - {}.mp3",
        OUTPUT_DIR,
        artists.join(", "),
        track_name
    );

    let status = Command::new("/opt/homebrew/bin/ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&tagged_ogg)
        .arg("-map_metadata")
        .arg("0:s:0")
        .arg("-write_id3v2")
        .arg("1")
        .arg("-id3v2_version")
        .arg("3")
        .arg("-b:a")
        .arg("192k")
        .arg(&output_mp3)
        .status()?;

    if !status.success() {
        return Err("ffmpeg failed".into());
    }

    std::fs::remove_file(tagged_ogg)?;
    std::fs::remove_file(ogg_path)?;

    info!("Done, written to: {}", output_mp3);
    Ok(())
}

#[tokio::main]
async fn main() {
    Builder::from_env(Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let url_input = cli.group.url.as_deref().unwrap();

    let spotify_url = Regex::new(r"open\.spotify\.com/track/([[:alnum:]]+)").unwrap();
    let caps = match spotify_url.captures(url_input) {
        Some(c) => c,
        None => {
            error!("Only Spotify track URLs are supported");
            return;
        }
    };

    let track_id = caps.get(1).unwrap().as_str();
    let spotify_uri = format!("spotify:track:{track_id}");

    let session_config = SessionConfig::default();

    let cache = Cache::new(Some(CACHE), Some(CACHE), Some(CACHE_FILES), None).unwrap();
    let credentials = cache
        .credentials()
        .ok_or(Error::unavailable("credentials not cached"))
        .or_else(|_| {
            librespot_oauth::OAuthClientBuilder::new(
                &session_config.client_id,
                "http://127.0.0.1:8898/login",
                vec!["streaming"],
            )
            .open_in_browser()
            .build()?
            .get_access_token()
            .map(|t| Credentials::with_access_token(t.access_token))
        }).unwrap();

    let session = Session::new(session_config, Some(cache));
    match session.connect(credentials, true).await {
        Ok(()) => info!("Session username: {:#?}", session.username()),
        Err(e) => {
            println!("Error connecting: {e}");
            return;
        }
    };

    if let Err(e) = process_track(&spotify_uri, &session).await {
        error!("Failed: {e}");
    }

}
