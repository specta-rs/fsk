use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, LocalFile, Metadata, NetworkFile,
    NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result, StatFs,
};
use std::path::PathBuf;

pub struct ExampleNetworkFilesystem {
    remote: Vec<u8>,
    downloaded: Vec<u8>,
    downloaded_inode: u64,
    downloaded_path: PathBuf,
}

impl Filesystem for ExampleNetworkFilesystem {
    fn statfs(&self) -> Result<StatFs> {
        let used = u64::try_from(self.remote.len() + self.downloaded.len()).unwrap_or(u64::MAX);
        Ok(StatFs {
            block_size: 4096,
            io_size: 131_072,
            total_bytes: used,
            free_bytes: 0,
            available_bytes: 0,
            files: 3,
            free_files: 0,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        let (kind, size, mode) = match inode {
            ROOT_INODE => (FileType::Directory, 0, 0o755),
            2 => (
                FileType::File,
                u64::try_from(self.remote.len()).unwrap_or(u64::MAX),
                0o444,
            ),
            3 => (
                FileType::File,
                u64::try_from(self.downloaded.len()).unwrap_or(u64::MAX),
                0o444,
            ),
            _ => return Err(Error(2)),
        };
        Ok(Metadata {
            inode,
            parent: ROOT_INODE,
            size,
            allocated_size: size,
            generation: 1,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode,
            uid: 0,
            gid: 0,
            link_count: 1,
            kind,
        })
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        if parent != ROOT_INODE {
            return Err(Error(20));
        }
        match name {
            b"origin.txt" => Ok((2, FileType::File)),
            b"downloaded.txt" => Ok((3, FileType::File)),
            _ => Err(Error(2)),
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
        let entries: &[(&[u8], u64)] = &[(b"origin.txt", 2), (b"downloaded.txt", 3)];
        let start = usize::try_from(cookie).map_err(|_| Error::INVALID)?;
        for (index, &(name, child)) in entries.iter().enumerate().skip(start) {
            if !sink.push(DirectoryEntry {
                name,
                inode: child,
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
            next_cookie: u64::try_from(entries.len()).unwrap_or(u64::MAX),
            eof: true,
        })
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        let data = match inode {
            2 => self.remote.as_slice(),
            3 => self.downloaded.as_slice(),
            _ => return Err(Error(2)),
        };
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let Some(input) = data.get(start..) else {
            return Ok(0);
        };
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        Ok(count)
    }
}

impl NetworkFilesystem for ExampleNetworkFilesystem {
    fn open_file(&self, inode: u64, _writable: bool) -> Result<NetworkFile> {
        if inode == self.downloaded_inode {
            return Ok(NetworkFile::Local(LocalFile::new(
                self.downloaded_path.clone(),
                1,
            )));
        }
        Ok(NetworkFile::Remote)
    }

    fn close_file(&self, _inode: u64, _file: NetworkFile) -> Result<()> {
        // Update last-access state or schedule eviction here. Swift has already
        // closed its local descriptor when this callback runs.
        Ok(())
    }
}

/// Create the example network backend.
///
/// # Errors
///
/// Returns an error if its starter files cannot be created.
pub fn filesystem(resource: &[u8]) -> Result<ExampleNetworkFilesystem> {
    let downloaded = b"This file is read locally.\n".to_vec();
    let downloaded_path = std::env::temp_dir().join("fsk-downloaded-example.txt");
    std::fs::write(&downloaded_path, &downloaded)
        .map_err(|error| fsk::Error(error.raw_os_error().unwrap_or(5)))?;

    Ok(ExampleNetworkFilesystem {
        remote: resource.to_vec(),
        downloaded,
        downloaded_inode: 3,
        downloaded_path,
    })
}
