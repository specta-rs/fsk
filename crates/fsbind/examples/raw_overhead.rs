use fsbind::{
    DirectorySink, FileType, Filesystem, MemoryFileSystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, StatFs,
    raw::{Caller, NativeRequest, Operation, Platform, RawEvent, RawEventMask, RawOutcome},
};
use std::{
    hint::black_box,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

struct ObservedFilesystem {
    inner: MemoryFileSystem,
    events: AtomicU64,
}

impl Filesystem for ObservedFilesystem {
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

    fn raw_event_mask(&self) -> RawEventMask {
        RawEventMask::operation(Operation::Lookup)
    }

    fn raw_event(&self, _event: RawEvent<'_>) -> RawOutcome {
        self.events.fetch_add(1, Ordering::Relaxed);
        RawOutcome::Continue
    }
}

fn timed(iterations: u64, mut operation: impl FnMut()) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    f64::from(u32::try_from(iterations).unwrap()) / start.elapsed().as_secs_f64()
}

#[inline]
fn adapter_lookup(filesystem: &dyn Filesystem, mask: RawEventMask) {
    if mask.contains(Operation::Lookup) {
        let outcome = filesystem.raw_event(RawEvent {
            platform: Platform::MacOS,
            operation: Operation::Lookup,
            caller: Caller::default(),
            native: NativeRequest::MacOS {
                request_id: 1,
                item_id: ROOT_INODE,
                flags: 0,
                payload: b"entry",
            },
            buffer: None,
        });
        black_box(outcome);
    }
    black_box(filesystem.lookup(ROOT_INODE, b"entry").unwrap());
}

fn main() {
    let plain = MemoryFileSystem::new();
    plain
        .create(ROOT_INODE, b"entry", FileType::File, 0o644)
        .unwrap();
    let observed_inner = MemoryFileSystem::new();
    observed_inner
        .create(ROOT_INODE, b"entry", FileType::File, 0o644)
        .unwrap();
    let observed = ObservedFilesystem {
        inner: observed_inner,
        events: AtomicU64::new(0),
    };
    let iterations = 10_000_000;

    let direct = timed(iterations, || {
        black_box(plain.lookup(ROOT_INODE, b"entry").unwrap());
    });
    let disabled_mask = plain.raw_event_mask();
    let disabled = timed(iterations, || adapter_lookup(&plain, disabled_mask));
    let enabled_mask = observed.raw_event_mask();
    let enabled = timed(iterations, || adapter_lookup(&observed, enabled_mask));

    println!("direct:       {direct:.0} lookups/s");
    println!(
        "raw disabled: {disabled:.0} lookups/s ({:.2}% vs direct)",
        (disabled / direct - 1.0) * 100.0
    );
    println!(
        "raw enabled:  {enabled:.0} lookups/s ({:.2}% vs direct, {} events)",
        (enabled / direct - 1.0) * 100.0,
        observed.events.load(Ordering::Relaxed)
    );
}
