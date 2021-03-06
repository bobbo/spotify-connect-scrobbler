use std::time::{Duration, Instant};

use futures::{Future, BoxFuture, Async, Poll};
use futures::future;
use rustfm_scrobble::{self, Scrobble};

use metadata::{Track, Artist, Album, Metadata};
use core::session::Session;
use core::util::SpotifyId;

#[derive(Clone, Debug)]
pub struct ScrobblerConfig {
    pub api_key: String,
    pub api_secret: String,
    pub username: String,
    pub password: String,
}

pub struct Scrobbler {
    config: ScrobblerConfig,
    scrobbler: rustfm_scrobble::Scrobbler,

    session: Box<Session>,
    current_track_id: Option<SpotifyId>,
    current_track_start: Option<Instant>,
    current_track_meta: Option<Scrobble>,
    current_track_scrobbled: bool,

    auth_future: BoxFuture<(), rustfm_scrobble::ScrobblerError>,
    new_track_future: BoxFuture<(), ()>,
    now_playing_future: BoxFuture<(), ScrobbleError>,
    meta_fetch_future: BoxFuture<Scrobble, ScrobbleError>,
    scrobble_future: Option<BoxFuture<(), ScrobbleError>>
}

#[derive(Debug)]
pub struct ScrobbleError {
    msg: String
}

impl ScrobbleError {

    pub fn new(msg: String) -> ScrobbleError {
        ScrobbleError {
            msg: msg
        }
    }

}

impl Scrobbler {

    pub fn new(config: ScrobblerConfig, session: Session) -> Scrobbler {
        let mut scrobbler = Scrobbler {
            session: Box::new(session),
            scrobbler: rustfm_scrobble::Scrobbler::new(&config.api_key, &config.api_secret),
            current_track_id: None,
            current_track_start: None,
            current_track_meta: None,
            current_track_scrobbled: false,
            auth_future: future::empty().boxed(),
            new_track_future: future::empty().boxed(),
            now_playing_future: future::empty().boxed(),
            meta_fetch_future: future::empty().boxed(),
            scrobble_future: None,
            config: config
        };

        scrobbler.start_auth();
        scrobbler
    }

    pub fn start_auth(&mut self) {
        self.auth_future = self.auth();
    }

    pub fn auth(&mut self) -> BoxFuture<(), rustfm_scrobble::ScrobblerError> {
        match self.scrobbler.authenticate_with_password(&self.config.username, &self.config.password) {
            Ok(_) => future::ok(()),
            Err(err) => future::err(err)
        }.boxed()
    }

    pub fn update_current_track(&mut self, track_id: SpotifyId, force_new_track: bool) {
        if !force_new_track {
            let mut new_track_detected = false;
            match self.current_track_id {
                None => { 
                    new_track_detected = true;
                },
                Some(id) => {
                    if id != track_id {
                        new_track_detected = true;
                    }
                }
            }

            if !new_track_detected {
                return
            }
        }

        if self.can_scrobble_track() {
            self.start_scrobble();
        }

        self.new_track_future = self.set_new_track(track_id);
    }

    pub fn set_new_track(&mut self, track_id: SpotifyId) -> BoxFuture<(), ()> {
        self.current_track_id = Some(track_id);
        self.current_track_start = Some(Instant::now());
        self.current_track_meta = None;
        self.current_track_scrobbled = false;

        future::ok(()).boxed()
    }

    pub fn get_track_meta(&mut self, track_id: SpotifyId) -> BoxFuture<Scrobble, ScrobbleError> {
        let session = self.session.clone();

        Track::get(&session, track_id).and_then(move |track| {
            let track_name = track.clone().name;
            let artist = *track.artists.first().expect("No artists");
            Artist::get(&session, artist).map(move |artist| (track_name, artist.name.clone(), track, session))
        }).and_then(move |(track_name, artist_name, track_meta, session)| {
            Album::get(&session, track_meta.album).map(|album| (track_name, artist_name, album.name.clone()))
        }).map_err(move |err| {
            ScrobbleError::new(format!("{:?}", err).to_owned())
        }).and_then(move |(track, artist, album)| {
            future::ok(Scrobble::new(&artist, &track, &album))
        }).boxed()
    }

    pub fn send_now_playing(&self, track: &Scrobble) -> BoxFuture<(), ScrobbleError> {
        info!("Now-playing scrobble: {:?}", track);

        match self.scrobbler.now_playing(track) {
            Ok(_) => future::ok(()),
            Err(err) => future::err(ScrobbleError::new(format!("{:?}", err)))
        }.boxed()
    }

    pub fn start_scrobble(&mut self) {
        self.scrobble_future = match self.current_track_meta {
            Some(ref meta) => {
                let scrobble = &meta.clone();
                Some(self.send_scrobble(scrobble))
            },
            None => {
                error!("No track meta-data available for scrobble");
                None
            }
        }
    }

    pub fn send_scrobble(&self, scrobble: &Scrobble) -> BoxFuture<(), ScrobbleError> {
        info!("Scrobbling: {:?}", scrobble);

        match self.scrobbler.scrobble(scrobble) {
            Ok(_) => future::ok(()),
            Err(err) => future::err(ScrobbleError::new(format!("{:?}", err)))
        }.boxed()
    }

    fn can_scrobble_track(&self) -> bool {
        if self.current_track_scrobbled {
            return false
        }

        match self.scrobble_future {
            Some(_) => {
                return false
            },
            None => {}
        }

        match self.current_track_start {
            Some(start_time) => {
                let play_time = start_time.elapsed();
                
                if play_time > Duration::new(20, 0) {
                    return true
                }

                false
            },
            _ => false
        }
    }

}

impl Future for Scrobbler {
    type Item = Result<(), ()>;
    type Error = ();

    fn poll(&mut self) -> Poll<Result<(), ()>, ()> {

        match self.auth_future.poll() {
            Ok(Async::Ready(_)) => {
                info!("Authenticated with Last.fm");
                self.auth_future = future::empty().boxed();
            },
            Ok(Async::NotReady) => {
            },
            Err(err) => {
                error!("Authentication error: {:?}", err);
                return Err(())
            }
        }

        if self.can_scrobble_track() {
            self.start_scrobble();
        }

        let mut track_scrobbled = false;
        match self.scrobble_future {
            Some(ref mut scrobble_future) => {
                match scrobble_future.poll() {
                    Ok(Async::Ready(_)) => {
                        track_scrobbled = true;
                    },
                    Ok(Async::NotReady) => {
                        return Ok(Async::NotReady)
                    },
                    Err(err) => {
                        error!("Scrobbling error: {:?}", err);
                        return Err(())
                    }
                }
            },
            None => ()
        }

        if track_scrobbled {
            self.scrobble_future = None;
            self.current_track_scrobbled = true;
        }

        match self.new_track_future.poll() {
            Ok(Async::Ready(_)) => {
                self.new_track_future = future::empty().boxed();
                self.current_track_scrobbled = false;

                match self.current_track_id {
                    Some(track_id) => {
                        self.meta_fetch_future = self.get_track_meta(track_id);
                    },
                    None => {

                    }
                }
            },
            Ok(Async::NotReady) => {

            },
            Err(err) => {
                error!("Failed to set new current track: {:?}", err);
                return Err(())
            }
        }

        match self.meta_fetch_future.poll() {
            Ok(Async::Ready(ref track)) => {
                self.meta_fetch_future = future::empty().boxed();
                self.now_playing_future = self.send_now_playing(track);
                self.current_track_meta = Some(track.clone());
            },
            Ok(Async::NotReady) => {
                
            },
            Err(err) => {
                error!("Metadata fetch error: {:?}", err);
                return Err(())
            }
        }

        match self.now_playing_future.poll() {
            Ok(Async::Ready(_)) => {
                self.now_playing_future = future::empty().boxed();
            },
            Ok(Async::NotReady) => {
                
            },
            Err(err) => {
                error!("Now Playing error: {:?}", err);
                return Err(())
            }
        }

        Ok(Async::NotReady)
    }

}
