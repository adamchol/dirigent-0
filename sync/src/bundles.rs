//! A snapshot's bundle as the log's rows hold it: base64, split into parts that each fit in one
//! row, and joined again as they're read.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use dirigent_protocol::Event;

use crate::Snapshot;

/// The base64 characters a row carries at most: well within a Kafka message (1 MB by default).
pub const PART: usize = 512 * 1024;

/// A commit's whole bundle, as read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    pub commit: String,
    pub parent: Option<String>,
    pub bundle: Vec<u8>,
}

/// `snapshot` as the log's rows: its bundle in parts of at most [`PART`] characters.
pub fn events(snapshot: &Snapshot) -> Vec<Event> {
    // whole groups of 3 bytes, so each part is base64 of its own
    let chunks: Vec<&[u8]> = if snapshot.bundle.is_empty() {
        vec![&[]]
    } else {
        snapshot.bundle.chunks(PART / 4 * 3).collect()
    };
    let parts = chunks.len() as u32;
    chunks
        .into_iter()
        .enumerate()
        .map(|(part, chunk)| Event::FilesChanged {
            commit: snapshot.commit.clone(),
            parent: snapshot.parent.clone(),
            part: part as u32,
            parts,
            bundle: STANDARD.encode(chunk),
        })
        .collect()
}

/// Joins a commit's parts as they come, in the log's order.
#[derive(Default)]
pub struct Bundles {
    partial: HashMap<String, Vec<Option<Vec<u8>>>>,
}

impl Bundles {
    /// The bundle `event` completes, if it does. A part that's malformed, or another commit's
    /// event, completes nothing; so a commit missing parts never comes out.
    pub fn push(&mut self, event: &Event) -> Option<Bundle> {
        let Event::FilesChanged {
            commit,
            parent,
            part,
            parts,
            bundle,
        } = event
        else {
            return None;
        };
        let (part, parts) = (*part as usize, *parts as usize);
        if part >= parts {
            return None;
        }
        let bytes = STANDARD.decode(bundle).ok()?;
        let got = self
            .partial
            .entry(commit.clone())
            .or_insert_with(|| vec![None; parts]);
        if got.len() != parts {
            return None;
        }
        got[part] = Some(bytes);
        if got.iter().any(Option::is_none) {
            return None;
        }
        let got = self.partial.remove(commit)?;
        Some(Bundle {
            commit: commit.clone(),
            parent: parent.clone(),
            bundle: got.into_iter().flatten().flatten().collect(),
        })
    }
}
