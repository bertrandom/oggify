extern crate env_logger;
extern crate librespot_audio;
extern crate librespot_core;
extern crate librespot_metadata;
#[macro_use]
extern crate log;
extern crate regex;
extern crate scoped_threadpool;
extern crate tokio;

use std::env;
use std::io::Write;
use std::io::{self, BufRead, Read, Result};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use env_logger::{Builder, Env};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::authentication::Credentials;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_id::SpotifyId;
use librespot_core::{SpotifyUri};

use librespot_core::cache::Cache;
use librespot_core::Error;


use librespot_metadata::{Album, Artist, Metadata, Track};
use librespot_metadata::audio::{AudioFileFormat};
use regex::Regex;
use scoped_threadpool::Pool;

// Read and write vorbiscomment metadata
use oggvorbismeta::{
    read_comment_header, replace_comment_header, CommentHeader, VorbisComments,
};
use std::fs::File;
use std::io::Cursor;

use clap::{Parser, Subcommand};

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

#[tokio::main]
async fn main() {
  Builder::from_env(Env::default().default_filter_or("info")).init();

  let cli = Cli::parse();

  let args: Vec<_> = env::args().collect();


  let url_input = cli.group.url.as_deref().unwrap();

  info!(
    "URL: {}",
    url_input
  );
  
  let spotify_url = Regex::new(r"open\.spotify\.com/track/([[:alnum:]]+)").unwrap();
  if !spotify_url.is_match(url_input) {
      error!("Only Spotify track URLs are supported currently.");
      return;
  }

  let track_id = spotify_url
      .captures(url_input)
      .and_then(|cap| cap.get(1))
      .map(|m| m.as_str())
      .unwrap();

  info!(
    "Track ID: {}",
    track_id
  );
  
  let spotify_uri = format!("spotify:track:{}", track_id);

  info!(
    "Spotify URI: {}",
    spotify_uri
  );

  // let core = tokio::runtime::Runtime::new().unwrap();
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
  info!("Connecting ...");

  match session.connect(credentials, true).await {
      Ok(()) => info!("Session username: {:#?}", session.username()),
      Err(e) => {
          println!("Error connecting: {e}");
          return;
      }
  };
  
  info!("Connected!");

  let mut threadpool = Pool::new(1);

  let uri = SpotifyUri::from_uri(&spotify_uri).unwrap();

  let id_str = uri.to_id().unwrap();

  let id = SpotifyId::from_base62(&id_str).unwrap();

  info!("Getting track metadata...");
  info!("Track URI: {}", uri);

  let track = Track::get(&session, &uri).await.unwrap();

  let artists = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>();
  info!("Artists: {}", artists.join(", "));
  
  let track_name = track.name.clone();
  info!("Track name: {}", track_name);

  let track_id = track.id.to_base62().unwrap();
  info!("Track id: {}", track_id);

  info!(
    "File formats: {}",
    track
      .files
      .keys()
      .map(|filetype| format!("{:?}", filetype))
      .collect::<Vec<_>>()
      .join(" ")
  );

  let file_id = track
    .files
    .get(&AudioFileFormat::OGG_VORBIS_320)
    .or(track.files.get(&AudioFileFormat::OGG_VORBIS_160))
    .or(track.files.get(&AudioFileFormat::OGG_VORBIS_96))
    .expect("Could not find a OGG_VORBIS format for the track.");

  let key = match session.audio_key().request(id, *file_id).await {
      Ok(key) => Some(key),
      Err(e) => {
          warn!("Unable to load key, continuing without decryption: {e}");
          None
      }
  };

  let fname = format!("{}/{}.ogg", OUTPUT_DIR, track_id);
  info!("Writing decrypted track to {}", fname);

  let mut encrypted_file = AudioFile::open(&session, *file_id, 320).await
    .unwrap();

  let mut buffer = Vec::new();
  let mut read_all: Result<usize> = Ok(0);
  let fetched = AtomicBool::new(false);
  threadpool.scoped(|scope| {
    scope.execute(|| {
      read_all = encrypted_file.read_to_end(&mut buffer);
      fetched.store(true, Ordering::Release);
    });
    while !fetched.load(Ordering::Acquire) {
      // tokio::time::sleep(Duration::from_millis(100)).await;
    }
  });

  read_all.expect("Cannot read file stream");
  let mut decrypted_buffer = Vec::new();
  AudioDecrypt::new(key, &buffer[..])
    .read_to_end(&mut decrypted_buffer)
    .expect("Cannot decrypt stream");

  std::fs::write(&fname, &decrypted_buffer[0xa7..]).expect("Cannot write decrypted track");

  let mut f_in_disk = File::open(fname).expect("Can't open file");
  let mut f_in_ram: Vec<u8> = vec![];

  std::io::copy(&mut f_in_disk, &mut f_in_ram).unwrap();
  
  let file_out = format!("{}/{}-tagged.ogg", OUTPUT_DIR, track_id);

  let f_in = Cursor::new(&f_in_ram);
  let mut new_comment = CommentHeader::new();

  new_comment.set_vendor("Ogg");
  for artist in &artists {
      new_comment.add_tag_single("artist", artist.to_string());
  }

  new_comment.add_tag_single("album", track.album.name.to_string());
  new_comment.add_tag_single("tracknumber", track.number.to_string());
  new_comment.add_tag_single("title", track_name.to_string());
  // Add year from date
  new_comment.add_tag_single("date", track.album.date.year().to_string());

  let tag_names = new_comment.get_tag_names();
  info!("New tags: {tag_names:?}");
  for tag in &tag_names {
      info!("New tag: {}, {:?}", tag, new_comment.get_tag_multi(tag));
  }

  info!("Insert new comments");
  let mut f_out = replace_comment_header(f_in, &new_comment).expect("Can't write comments");

  info!("Save to disk");
  let mut f_out_disk = File::create(file_out).unwrap();
  std::io::copy(&mut f_out, &mut f_out_disk).unwrap();

  let ffmpeg_cmd = format!(
      "/opt/homebrew/bin/ffmpeg -i {}/{}-tagged.ogg -map_metadata 0:s:0 -write_id3v2 1 -id3v2_version 3 {}/{}.mp3",
      OUTPUT_DIR, track_id, OUTPUT_DIR, track_id
  );

  let mut cmd = Command::new("/opt/homebrew/bin/ffmpeg");

  let output_mp3 = format!("{}/{} - {}.mp3", OUTPUT_DIR, artists.join(", "), track_name.to_string());

  cmd.arg("-y")
    .arg("-i")
    .arg(format!("{}/{}-tagged.ogg", OUTPUT_DIR, track_id))
    .arg("-map_metadata")
    .arg("0:s:0")
    .arg("-write_id3v2")
    .arg("1")
    .arg("-id3v2_version")
    .arg("3")
    .arg("-b:a")
    .arg("192k")
    .arg(output_mp3.clone());

  cmd.stdin(Stdio::piped());

  let mut child = cmd.spawn().expect("Could not run helper program");
  assert!(
    child
      .wait()
      .expect("Out of ideas for error messages")
      .success(),
    "Helper script returned an error"
  );

  // Remove the tagged ogg file
  let tagged_ogg_file = format!("{}/{}-tagged.ogg", OUTPUT_DIR, track_id);
  std::fs::remove_file(tagged_ogg_file).expect("Could not remove tagged ogg file");

  // Remove the original ogg file
  let original_ogg_file = format!("{}/{}.ogg", OUTPUT_DIR, track_id);
  std::fs::remove_file(original_ogg_file).expect("Could not remove original ogg file");

  info!("Done, written to: {}", output_mp3);

}