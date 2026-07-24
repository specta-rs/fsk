use fsbind::{
    DirectorySink, FileType, Filesystem, LocalFile, MemoryFileSystem, Metadata, NetworkFile,
    NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result, StatFs,
};
use std::path::PathBuf;

pub struct ExampleNetworkFilesystem {
    inner: MemoryFileSystem,
    downloaded_inode: u64,
    downloaded_path: PathBuf,
}

impl Filesystem for ExampleNetworkFilesystem {
    fn statfs(&self) -> Result<StatFs> {
        self.inner.statfs()
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        self.inner.metadata(inode)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        self.inner.lookup(parent, name)
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        self.inner.read_directory(inode, cookie, verifier, sink)
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        self.inner.read(inode, offset, output)
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
    let inner = MemoryFileSystem::new();
    let remote = inner.create(ROOT_INODE, b"origin.txt", FileType::File, 0o444)?;
    inner.write(remote, 0, resource)?;

    let downloaded_inode = inner.create(ROOT_INODE, b"downloaded.txt", FileType::File, 0o444)?;
    let downloaded_path = std::env::temp_dir().join("fsbind-downloaded-example.txt");
    std::fs::write(&downloaded_path, b"This file is read locally.\n")
        .map_err(|error| fsbind::Error(error.raw_os_error().unwrap_or(5)))?;

    Ok(ExampleNetworkFilesystem {
        inner,
        downloaded_inode,
        downloaded_path,
    })
}
