use fsk_windows_logic_audit::*;
use fsk::{DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ReadDirectoryResult, Result, StatFs, ROOT_INODE};
use std::{
    sync::{Arc, Mutex},
};

struct CaseSensitiveBackend;

impl Filesystem for CaseSensitiveBackend {
    fn statfs(&self) -> Result<StatFs> {
        Ok(StatFs::default())
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        let (parent, kind) = match inode {
            ROOT_INODE => (ROOT_INODE, FileType::Directory),
            2 => (ROOT_INODE, FileType::File),
            3 => (ROOT_INODE, FileType::Directory),
            4 => (3, FileType::File),
            _ => return Err(Error::NOT_FOUND),
        };
        let mut metadata = if kind == FileType::Directory {
            Metadata::directory(inode, parent, 0o755)
        } else {
            Metadata::file(inode, parent, 0, 0o644)
        };
        metadata.generation = 1;
        Ok(metadata)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        match (parent, name) {
            (ROOT_INODE, b"File.txt") => Ok((2, FileType::File)),
            (ROOT_INODE, b"Dir") => Ok((3, FileType::Directory)),
            (3, b"File.txt") => Ok((4, FileType::File)),
            _ => Err(Error::NOT_FOUND),
        }
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        _verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        match (inode, cookie) {
            (ROOT_INODE, 0) => {
                sink.push(DirectoryEntry {
                    name: b"File.txt",
                    inode: 2,
                    kind: FileType::File,
                    next_cookie: 1,
                });
                sink.push(DirectoryEntry {
                    name: b"Dir",
                    inode: 3,
                    kind: FileType::Directory,
                    next_cookie: 1,
                });
            }
            (ROOT_INODE, 1) | (3, 1) => {}
            (3, 0) => {
                sink.push(DirectoryEntry {
                    name: b"File.txt",
                    inode: 4,
                    kind: FileType::File,
                    next_cookie: 1,
                });
            }
            _ => return Err(Error::INVALID),
        }
        Ok(ReadDirectoryResult {
            verifier: 1,
            next_cookie: 1,
            eof: true,
        })
    }

    fn read(&self, _inode: u64, _offset: u64, _output: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
}

struct SymlinkBackend {
    short_buffer_error: Error,
    capacities: Mutex<Vec<usize>>,
}

impl Filesystem for SymlinkBackend {
    fn statfs(&self) -> Result<StatFs> {
        Ok(StatFs::default())
    }

    fn metadata(&self, _inode: u64) -> Result<Metadata> {
        Err(Error::NOT_FOUND)
    }

    fn lookup(&self, _parent: u64, _name: &[u8]) -> Result<(u64, FileType)> {
        Err(Error::NOT_FOUND)
    }

    fn read_directory(
        &self,
        _inode: u64,
        _cookie: u64,
        _verifier: u64,
        _sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        Err(Error::INVALID)
    }

    fn read(&self, _inode: u64, _offset: u64, _output: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn read_symlink(&self, _inode: u64, output: &mut [u8]) -> Result<usize> {
        self.capacities.lock().unwrap().push(output.len());
        let target = b"../target";
        if output.len() < target.len() {
            return Err(self.short_buffer_error);
        }
        output[..target.len()].copy_from_slice(target);
        Ok(target.len())
    }
}

fn state() -> WindowsState {
    WindowsState {
        filesystem: Arc::new(CaseSensitiveBackend),
        api: Api,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

#[test]
fn windows_path_lookup_uses_projfs_name_matching() {
    assert_eq!(state().inode_for_path(&wide("file.txt")), Ok(2));
    assert_eq!(state().inode_for_path(&wide("dir/file.txt")), Ok(4));
}

#[test]
fn windows_path_lookup_keeps_exact_and_missing_behavior() {
    assert_eq!(state().inode_for_path(&wide("File.txt")), Ok(2));
    assert_eq!(state().inode_for_path(&wide("Dir/File.txt")), Ok(4));
    assert_eq!(state().inode_for_path(&wide("")), Ok(ROOT_INODE));
    assert_eq!(
        state().inode_for_path(&wide("missing.txt")),
        Err(Error::NOT_FOUND)
    );
}

#[test]
fn existing_virtualization_root_is_accepted() {
    assert!(check_placeholder_mark_hresult(hresult(ERROR_REPARSE_POINT_ENCOUNTERED)).is_ok());
    assert!(check_hresult(hresult(ERROR_REPARSE_POINT_ENCOUNTERED)).is_err());
}

#[test]
fn symlink_metadata_has_the_windows_reparse_attribute() {
    let mut metadata = Metadata::file(2, ROOT_INODE, 4, 0o777);
    metadata.kind = FileType::Symlink;
    let info = basic_info(metadata);
    assert_ne!(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    assert_eq!(info.FileAttributes & FILE_ATTRIBUTE_NORMAL, 0);
}

#[test]
fn symlink_target_retries_portable_short_buffer_errors() {
    for short_buffer_error in [Error::RANGE, Error::INVALID] {
        let backend = SymlinkBackend {
            short_buffer_error,
            capacities: Mutex::new(Vec::new()),
        };
        let mut metadata = Metadata::file(2, ROOT_INODE, 1, 0o777);
        metadata.kind = FileType::Symlink;

        let target = read_symlink_target(&backend, metadata).unwrap();
        let expected: Vec<u16> = "../target\0".encode_utf16().collect();
        assert_eq!(target, Some(expected));
        assert_eq!(*backend.capacities.lock().unwrap(), vec![1, 2, 4, 8, 16]);
    }
}

#[test]
fn file_directory_and_read_only_attributes_remain_correct() {
    let file = basic_info(Metadata::file(2, ROOT_INODE, 10, 0o644));
    assert!(!file.IsDirectory);
    assert_eq!(file.FileSize, 10);
    assert_eq!(file.FileAttributes, FILE_ATTRIBUTE_NORMAL);

    let directory = basic_info(Metadata::directory(3, ROOT_INODE, 0o755));
    assert!(directory.IsDirectory);
    assert_eq!(directory.FileAttributes, FILE_ATTRIBUTE_DIRECTORY);

    let read_only = basic_info(Metadata::file(4, ROOT_INODE, 10, 0o444));
    assert_eq!(read_only.FileAttributes, FILE_ATTRIBUTE_READ_ONLY);
}

#[test]
fn wtf8_round_trips_every_unicode_scalar_and_unpaired_surrogate() {
    for scalar in 0..=0x10_ffff {
        if (0xd800..=0xdfff).contains(&scalar) {
            continue;
        }
        let units = if scalar <= 0xffff {
            vec![scalar as u16]
        } else {
            let value = scalar - 0x1_0000;
            vec![0xd800 | (value >> 10) as u16, 0xdc00 | (value & 0x3ff) as u16]
        };
        assert_eq!(decode_wtf8(&encode_wtf8(&units)), units, "U+{scalar:04X}");
    }
    for surrogate in 0xd800..=0xdfff {
        let units = [surrogate];
        assert_eq!(decode_wtf8(&encode_wtf8(&units)), units);
    }
}

#[test]
fn hydration_resumes_short_reads_and_rejects_invalid_counts() {
    let source = *b"abcdefghij";
    let mut output = [0_u8; 10];
    let count = hydrate(0, &mut output, |position, target| {
        let start = position as usize;
        let count = source[start..].len().min(3).min(target.len());
        target[..count].copy_from_slice(&source[start..start + count]);
        Ok(count)
    })
    .unwrap();
    assert_eq!(count, source.len());
    assert_eq!(output, source);
    assert_eq!(
        hydrate(0, &mut [0_u8; 4], |_, target| Ok(target.len() + 1)),
        Err(Error::INVALID)
    );
}

#[test]
fn windows_time_uses_100ns_ticks_since_1601() {
    assert_eq!(windows_time(0), WINDOWS_EPOCH_OFFSET_100NS as i64);
    assert_eq!(windows_time(100), WINDOWS_EPOCH_OFFSET_100NS as i64 + 1);
    assert_eq!(windows_time(99), WINDOWS_EPOCH_OFFSET_100NS as i64);
}
