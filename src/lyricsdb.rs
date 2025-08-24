// lyricsdb.rs: Simple local lyrics database (JSON file, only stores synced lyrics)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct LyricsDB {
    // Key: "artist|title", Value: synced lyrics string
    pub entries: HashMap<String, String>,
}

impl LyricsDB {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn Error>> {
        if !path.as_ref().exists() {
            return Ok(LyricsDB::default());
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let db = serde_json::from_reader(reader)?;
        Ok(db)
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn Error>> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, self)?;
        Ok(())
    }

    pub fn get(&self, artist: &str, title: &str) -> Option<String> {
        let key = format!("{}|{}", artist.to_lowercase(), title.to_lowercase());
        self.entries.get(&key).cloned()
    }

    pub fn insert(&mut self, artist: &str, title: &str, synced: &str) {
        let key = format!("{}|{}", artist.to_lowercase(), title.to_lowercase());
        self.entries.insert(key, synced.to_string());
    }
}
