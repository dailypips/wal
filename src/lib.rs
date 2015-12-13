#![feature(slice_patterns)]

extern crate byteorder;
extern crate crc;
extern crate eventual;
extern crate fs2;
extern crate memmap;
extern crate rand;
extern crate time;

#[macro_use] extern crate log;

#[cfg(test)] extern crate quickcheck;
#[cfg(test)] extern crate env_logger;
#[cfg(test)] extern crate tempdir;

mod segment;
pub mod test_utils;

use std::cmp::Ordering;
use std::fmt;
use std::fs::{self, File};
use std::io::{Error, ErrorKind, Result};
use std::mem;
use std::ops;
use std::path::{Path, PathBuf};
use std::result;
use std::str::FromStr;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use fs2::FileExt;
use eventual::{Async, Future};

pub use segment::{Entry, Segment};

#[derive(Debug)]
pub struct WalOptions {
    /// The segment capacity. Defaults to 32MiB.
    pub segment_capacity: usize,

    /// The number of segments to create ahead of time, so that appends never
    /// need to wait on creating a new segment.
    pub segment_queue_len: usize,
}

impl Default for WalOptions {
    fn default() -> WalOptions {
        WalOptions {
            segment_capacity: 32 * 1024 * 1024,
            segment_queue_len: 0,
        }
    }
}

/// An open segment and its ID.
#[derive(Debug)]
struct OpenSegment {
    pub id: u64,
    pub segment: Segment,
}

/// A closed segment, and the associated start and stop indices.
#[derive(Debug)]
struct ClosedSegment {
    pub start_index: u64,
    pub segment: Segment,
}

enum WalSegment {
    Open(OpenSegment),
    Closed(ClosedSegment),
}

pub struct Wal {
    /// The segment currently being appended to.
    open_segment: OpenSegment,
    closed_segments: Vec<ClosedSegment>,
    creator: SegmentCreator,
    /// The directory containing the write ahead log. Used to hold an open file
    /// lock for the lifetime of the log.
    #[allow(dead_code)]
    dir: File,

    /// Keeps track of the flush completions of previously closed segments.
    flush: Option<Future<(), Error>>,
}

impl Wal {
    pub fn open<P>(path: P) -> Result<Wal> where P: AsRef<Path> {
        Wal::with_options(path, &WalOptions::default())
    }

    pub fn with_options<P>(path: P, options: &WalOptions) -> Result<Wal> where P: AsRef<Path> {
        let dir = try!(File::open(&path));
        try!(dir.try_lock_exclusive());

        // Holds open segments in the directory.
        let mut open_segments: Vec<OpenSegment> = Vec::new();
        let mut closed_segments: Vec<ClosedSegment> = Vec::new();

        for entry in try!(fs::read_dir(&path)) {
            match try!(open_dir_entry(try!(entry))) {
                WalSegment::Open(open_segment) => open_segments.push(open_segment),
                WalSegment::Closed(closed_segment) => closed_segments.push(closed_segment),
            }
        }

        // Validate the closed segments. They must be non-overlapping, and contiguous.
        closed_segments.sort_by(|a, b| a.start_index.cmp(&b.start_index));
        let mut next_start_index = 0;
        for &ClosedSegment { start_index, ref segment, .. } in &closed_segments {
            if start_index > next_start_index {
                return Err(Error::new(ErrorKind::InvalidData,
                                      format!("missing segment(s) containing wal entries {} to {}",
                                              next_start_index, start_index)));
            } else if start_index < next_start_index {
                // TODO: figure out what to do here.
                // Current thinking is the previous segment should be truncated.
                unimplemented!()
            }
            next_start_index += segment.len() as u64;
        }

        // Validate the open segments.
        open_segments.sort_by(|a, b| a.id.cmp(&b.id));

        // The latest open segment, may already have segments.
        let mut open_segment: Option<OpenSegment> = None;
        // Unused open segments.
        let mut unused_segments: Vec<OpenSegment> = Vec::new();

        for segment in open_segments {
            if segment.segment.len() > 0 {
                // This segment has already been written to. If a previous open
                // segment has also already been written to, we close it out and
                // replace it with this new one. This may happen because when a
                // segment is closed it is renamed, but the directory is not
                // sync'd, so the operation is not guaranteed to be durable.
                let stranded_segment = open_segment.take();
                open_segment = Some(segment);
                if let Some(segment) = stranded_segment {
                    let closed_segment = try!(close_segment(segment, next_start_index));
                    next_start_index += closed_segment.segment.len() as u64;
                    closed_segments.push(closed_segment);
                }
            } else if open_segment.is_none() {
                open_segment = Some(segment);
            } else {
                unused_segments.push(segment);
            }
        }

        let mut creator = SegmentCreator::new(&path,
                                              unused_segments,
                                              options.segment_capacity,
                                              options.segment_queue_len);

        let open_segment = match open_segment {
            Some(segment) => segment,
            None => try!(creator.next()),
        };

        Ok(Wal {
            open_segment: open_segment,
            closed_segments: closed_segments,
            creator: creator,
            dir: dir,
            flush: None,
        })
    }

    fn retire_open_segment(&mut self) -> Result<()> {
        // TODO: time the next call
        let mut segment = try!(self.creator.next());
        mem::swap(&mut self.open_segment, &mut segment);

        self.flush = Some(self.flush
                              .take()
                              .unwrap_or(Future::of(()))
                              .and(segment.segment.flush_async()));

        let start_index = self.open_segment_start_index();
        self.closed_segments.push(try!(close_segment(segment, start_index)));
        Ok(())
    }

    pub fn append<T>(&mut self, entry: &T) -> Result<u64> where T: ops::Deref<Target=[u8]> {
        if entry.len() > self.open_segment.segment.remaining_size() {
            try!(self.retire_open_segment());
        }

        // TODO: figure out a solution for entries bigger than the segment size.
        Ok(self.open_segment_start_index()
           + self.open_segment.segment.append(entry).unwrap() as u64)
    }

    /// Retrieve the entry with the provided index from the log.
    pub fn entry(&self, index: u64) -> Option<Entry> {
        let open_start_index = self.open_segment_start_index();
        if index >= open_start_index {
            return self.open_segment.segment.entry((index - open_start_index) as usize);
        }

        match self.find_closed_segment(index) {
            Ok(segment_index) => {
                let segment = &self.closed_segments[segment_index];
                segment.segment.entry((index - segment.start_index) as usize)
            },
            Err(i) => {
                // Sanity check that the missing index is less than the start of the log.
                assert_eq!(0, i);
                None
            }
        }
    }

    /// Truncates entries in the log beginning with `from`.
    ///
    /// Entries can be immediately appended to the log once this method returns,
    /// but the truncated entries are not guaranteed to be removed until the
    /// wal is flushed.
    pub fn truncate(&mut self, from: u64) {
        let open_start_index = self.open_segment_start_index();
        if from >= open_start_index {
            self.open_segment.segment.truncate((from - open_start_index) as usize);
        } else {
            // Truncate the open segment completely.
            self.open_segment.segment.truncate(0);

            match self.find_closed_segment(from) {
                Ok(index) => {
                    if from == self.closed_segments[index].start_index {
                        for segment in self.closed_segments.drain(index..) {
                            // TODO: this should be async
                            segment.segment.delete().unwrap();
                        }
                    } else {
                        {
                            let segment = &mut self.closed_segments[index];
                            segment.segment.truncate((from - segment.start_index) as usize)
                        }
                        if index + 1 < self.closed_segments.len() {
                            for segment in self.closed_segments.drain(index + 1..) {
                                // TODO: this should be async
                                segment.segment.delete().unwrap();
                            }
                        }
                    }
                },
                Err(index) => {
                    // The truncate index is before the first entry of the wal
                    assert!(from <= self.closed_segments.get(index)
                                                        .map(|segment| segment.start_index)
                                                        .unwrap_or(0));
                    for segment in self.closed_segments.drain(..) {
                        // TODO: this should be async
                        segment.segment.delete().unwrap();
                    }
                }
            }
        }
    }

    /// Returns the start index of the open segment.
    fn open_segment_start_index(&self) -> u64 {
        self.closed_segments
            .last()
            .map(|segment| segment.start_index + segment.segment.len() as u64)
            .unwrap_or(0)
    }

    fn find_closed_segment(&self, index: u64) -> result::Result<usize, usize> {
        let result = self.closed_segments.binary_search_by(|segment| {
            if index < segment.start_index { Ordering::Greater }
            else if index >= segment.start_index + segment.segment.len() as u64 { Ordering::Less }
            else { Ordering::Equal }
        });
        result
    }
}

impl fmt::Debug for Wal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Wal {{ segment_count: {} }}",
               self.closed_segments.len() + 1)
    }
}

fn close_segment(mut segment: OpenSegment, start_index: u64) -> Result<ClosedSegment> {
    let new_path = segment.segment.path().with_file_name(format!("closed-{}", start_index));
    debug!("closing segment {:?}, start index: {:?}", segment, new_path);
    try!(segment.segment.rename(new_path));
    Ok(ClosedSegment { start_index: start_index, segment: segment.segment })
}

fn open_dir_entry(entry: fs::DirEntry) -> Result<WalSegment> {
    let metadata = try!(entry.metadata());

    let error = || {
        Error::new(ErrorKind::InvalidData,
                   format!("unexpected entry in wal directory: {:?}", entry.path()))
    };

    if !metadata.is_file() {
        return Err(error());
    }

    let filename = try!(entry.file_name().into_string().map_err(|_| error()));
    match &*filename.split('-').collect::<Vec<&str>>() {
        ["open", id] => {
            let id = try!(u64::from_str(id).map_err(|_| error()));
            let segment = try!(Segment::open(entry.path()));
            Ok(WalSegment::Open(OpenSegment { segment: segment, id: id }))
        },
        ["closed", start] => {
            let start = try!(u64::from_str(start).map_err(|_| error()));
            let segment = try!(Segment::open(entry.path()));
            Ok(WalSegment::Closed(ClosedSegment { start_index: start, segment: segment }))
        },
        _ => Err(error()),
    }
}

struct SegmentCreator {
    /// Receive channel for new segments.
    rx: Option<Receiver<OpenSegment>>,
    /// The segment creator thread.
    ///
    /// Used for retrieving error upon failure.
    thread: Option<thread::JoinHandle<Result<()>>>
}

impl SegmentCreator {

    /// Creates a new segment creator.
    ///
    /// The segment creator must be started before new segments will be created.
    pub fn new<P>(dir: P,
                  existing: Vec<OpenSegment>,
                  segment_capacity: usize,
                  segment_queue_len: usize) -> SegmentCreator where P: AsRef<Path> {
        let (tx, rx) = sync_channel::<OpenSegment>(segment_queue_len);

        let dir = dir.as_ref().to_path_buf();
        let thread = thread::spawn(move || create_loop(tx, dir, segment_capacity, existing));
        SegmentCreator { rx: Some(rx), thread: Some(thread) }
    }

    /// Retrieves the next segment.
    pub fn next(&mut self) -> Result<OpenSegment> {
        self.rx.as_mut().unwrap().recv().map_err(|_| {
            match self.thread.take().map(|join_handle| join_handle.join()) {
                Some(Ok(Err(error))) => error,
                None => Error::new(ErrorKind::Other, "segment creator thread already failed"),
                Some(Ok(Ok(()))) => unreachable!("segment creator thread finished without an error,
                                                  but the segment creator is still live"),
                Some(Err(_)) => unreachable!("segment creator thread panicked"),
            }
        })
    }
}

impl Drop for SegmentCreator {
    fn drop(&mut self) {
        drop(self.rx.take());
        self.thread.take().map(|join_handle| {
            if let Err(error) = join_handle.join() {
                warn!("Error while shutting down segment creator: {:?}", error);
            }
        });
    }
}

fn create_loop(tx: SyncSender<OpenSegment>,
               mut path: PathBuf,
               capacity: usize,
               mut existing_segments: Vec<OpenSegment>) -> Result<()> {
    // Ensure the existing segments are in ID order.
    existing_segments.sort_by(|a, b| a.id.cmp(&b.id));

    let mut cont = true;
    let mut id = 0;

    for segment in existing_segments {
        id = segment.id;
        if let Err(_) = tx.send(segment) {
            cont = false;
            break;
        }
    }

    let dir = try!(File::open(&path));

    while cont {
        path.push(format!("open-{}", id));
        let segment = OpenSegment { id: id, segment: try!(Segment::create(&path, capacity)) };
        path.pop();
        id += 1;
        // Sync the directory, guaranteeing that the segment file is durably
        // stored on the filesystem.
        try!(dir.sync_all());
        cont = tx.send(segment).is_ok();
    }

    info!("segment creator shutting down");
    Ok(())
}

#[cfg(test)]
mod test {

    use std::error::Error;

    use env_logger;
    use fs2;
    use quickcheck;
    use quickcheck::TestResult;
    use tempdir;

    use super::{SegmentCreator, Wal, WalOptions};
    use test_utils::EntryGenerator;

    /// Check that entries appended to the write ahead log can be read back.
    #[test]
    fn check_wal() {
        let _ = env_logger::init();
        fn wal(entry_count: usize) -> TestResult {
            let dir = tempdir::TempDir::new("wal").unwrap();
            let mut wal = Wal::open(&dir.path()).unwrap();
            let entries = EntryGenerator::new().into_iter().take(entry_count).collect::<Vec<_>>();

            for entry in &entries {
                if let Err(error) = wal.append(entry) {
                    return TestResult::error(error.description());
                }
            }

            for (index, expected) in entries.iter().enumerate() {
                match wal.entry(index as u64) {
                    Some(ref entry) if &entry[..] != &expected[..] => return TestResult::failed(),
                    None => return TestResult::failed(),
                    _ => (),
                }
            }
            TestResult::passed()
        }

        quickcheck::quickcheck(wal as fn(usize) -> TestResult);
    }

    /// Check that the Wal will read previously written entries.
    #[test]
    fn check_wal_reopen() {
        let _ = env_logger::init();
        fn wal(entry_count: usize) -> TestResult {
            let entries = EntryGenerator::new().into_iter().take(entry_count).collect::<Vec<_>>();
            let dir = tempdir::TempDir::new("wal").unwrap();
            {
                let mut wal = Wal::open(&dir.path()).unwrap();
                for entry in &entries {
                    let _ = wal.append(entry);
                }
            }

            let wal = Wal::open(&dir.path()).unwrap();
            // Check that all of the entries are present.
            for (index, expected) in entries.iter().enumerate() {
                match wal.entry(index as u64) {
                    Some(ref entry) if &entry[..] != &expected[..] => return TestResult::failed(),
                    None => return TestResult::failed(),
                    _ => (),
                }
            }
            TestResult::passed()
        }

        quickcheck::quickcheck(wal as fn(usize) -> TestResult);
    }

    /// Check that entries appended to the write ahead log can be read back.
    #[test]
    fn check_truncate() {
        let _ = env_logger::init();
        fn truncate(entry_count: usize, truncate: usize) -> TestResult {
            if truncate > entry_count { return TestResult::discard(); }
            let dir = tempdir::TempDir::new("wal").unwrap();
            let mut wal = Wal::open(&dir.path()).unwrap();
            let entries = EntryGenerator::new().into_iter().take(entry_count).collect::<Vec<_>>();

            for entry in &entries {
                if let Err(error) = wal.append(entry) {
                    return TestResult::error(error.description());
                }
            }

            wal.truncate(truncate as u64);

            for (index, expected) in entries.iter().take(truncate).enumerate() {
                match wal.entry(index as u64) {
                    Some(ref entry) if &entry[..] != &expected[..] => return TestResult::failed(),
                    None => return TestResult::failed(),
                    _ => (),
                }
            }

            TestResult::from_bool(wal.entry(truncate as u64).is_none());

            TestResult::passed()
        }

        quickcheck::quickcheck(truncate as fn(usize, usize) -> TestResult);
    }

    #[test]
    fn test_append() {
        let _ = env_logger::init();
        let dir = tempdir::TempDir::new("wal").unwrap();
        let mut wal = Wal::open(&dir.path()).unwrap();

        let entry: &[u8] = &[42u8; 4096];
        for _ in 1..10 {
            wal.append(&entry).unwrap();
        }
    }

    #[test]
    fn test_truncate() {
        let _ = env_logger::init();
        let dir = tempdir::TempDir::new("wal").unwrap();
        // 2 entries should fit in each segment
        let mut wal = Wal::with_options(&dir.path(),
                                        &WalOptions { segment_capacity: 4096,
                                                      segment_queue_len: 3 }).unwrap();

        let entry: [u8; 2000] = [42u8; 2000];

        for truncate_index in 0..10 {
            println!("testing truncate_index {}", truncate_index);
            assert!(wal.entry(0).is_none());
            for i in 0..10 {
                assert_eq!(i, wal.append(&&entry[..]).unwrap());
            }

            wal.truncate(truncate_index);

            assert!(wal.entry(truncate_index).is_none());

            if truncate_index > 0 {
                assert!(wal.entry(truncate_index - 1).is_some());
            }
            wal.truncate(0);
        }
    }

    /// Tests that two Wal instances can not coexist for the same directory.
    #[test]
    fn test_exclusive_lock() {
        let _ = env_logger::init();
        let dir = tempdir::TempDir::new("wal").unwrap();
        let wal = Wal::open(&dir.path()).unwrap();
        assert_eq!(fs2::lock_contended_error().kind(),
                   Wal::open(&dir.path()).unwrap_err().kind());
        drop(wal);
        assert!(Wal::open(&dir.path()).is_ok());
    }

    #[test]
    fn test_segment_creator() {
        let _ = env_logger::init();
        let dir = tempdir::TempDir::new("segment").unwrap();
        let mut creator = SegmentCreator::new(&dir.path(), vec![], 1, 1024);

        for _ in 0..10 {
            let _ = creator.next();
        }
    }
}
