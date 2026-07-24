use std::sync::atomic::{AtomicU64, Ordering};
use ufsi::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, StatFs,
    raw::{RawEvent, RawEventMask, RawOutcome},
};

/// Minimal, immutable fixture for measuring `UFSI`'s interface costs.
///
/// This intentionally has no inode index, cache, locks, or consistency policy.
/// Setup allocations happen before timing begins.
pub struct BenchmarkFilesystem {
    payload: Vec<u8>,
    names: Vec<Vec<u8>>,
    observe_raw: bool,
    events: AtomicU64,
}

impl BenchmarkFilesystem {
    pub fn new(entry_count: usize, payload_size: usize, observe_raw: bool) -> Self {
        let names = (0..entry_count)
            .map(|index| format!("entry-{index:05}").into_bytes())
            .collect();
        Self {
            payload: vec![0x5a; payload_size],
            names,
            observe_raw,
            events: AtomicU64::new(0),
        }
    }

    #[allow(dead_code)]
    pub fn observed_events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }
}

impl Filesystem for BenchmarkFilesystem {
    fn statfs(&self) -> Result<StatFs> {
        Ok(StatFs {
            block_size: 4096,
            io_size: 128 * 1024,
            total_bytes: u64::try_from(self.payload.len()).unwrap_or(u64::MAX),
            free_bytes: 0,
            available_bytes: 0,
            files: u64::try_from(self.names.len() + 1).unwrap_or(u64::MAX),
            free_files: 0,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        Ok(Metadata {
            inode,
            parent: ROOT_INODE,
            size: u64::try_from(self.payload.len()).unwrap_or(u64::MAX),
            allocated_size: u64::try_from(self.payload.len()).unwrap_or(u64::MAX),
            generation: 1,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode: 0o444,
            uid: 0,
            gid: 0,
            link_count: 1,
            kind: FileType::File,
        })
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        if parent == ROOT_INODE && (name == b"entry" || name == b"entry-05000") {
            Ok((2, FileType::File))
        } else {
            Err(Error(2))
        }
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        _verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        if inode != ROOT_INODE {
            return Err(Error(20));
        }
        let start = usize::try_from(cookie).map_err(|_| Error::INVALID)?;
        for (index, name) in self.names.iter().enumerate().skip(start) {
            if !sink.push(DirectoryEntry {
                name,
                inode: u64::try_from(index + 2).unwrap_or(u64::MAX),
                kind: FileType::File,
                next_cookie: u64::try_from(index + 1).unwrap_or(u64::MAX),
            }) {
                return Ok(ReadDirectoryResult {
                    verifier: 1,
                    next_cookie: u64::try_from(index).unwrap_or(u64::MAX),
                    eof: false,
                });
            }
        }
        Ok(ReadDirectoryResult {
            verifier: 1,
            next_cookie: u64::try_from(self.names.len()).unwrap_or(u64::MAX),
            eof: true,
        })
    }

    fn read(&self, _inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let Some(input) = self.payload.get(start..) else {
            return Ok(0);
        };
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        Ok(count)
    }

    fn raw_event_mask(&self) -> RawEventMask {
        if self.observe_raw {
            RawEventMask::ALL
        } else {
            RawEventMask::NONE
        }
    }

    fn raw_event(&self, _event: RawEvent<'_>) -> RawOutcome {
        self.events.fetch_add(1, Ordering::Relaxed);
        RawOutcome::Continue
    }
}
