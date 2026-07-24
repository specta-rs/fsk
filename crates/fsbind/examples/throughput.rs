use fsbind::{DirectoryEntry, DirectorySink, FileType, Filesystem, MemoryFileSystem, ROOT_INODE};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

struct CountingSink(u32);

impl DirectorySink for CountingSink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        self.0 += 1;
        black_box(entry);
        true
    }
}

fn timed(iterations: u32, mut operation: impl FnMut()) -> (Duration, f64) {
    let start = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    let elapsed = start.elapsed();
    (elapsed, f64::from(iterations) / elapsed.as_secs_f64())
}

fn main() {
    let filesystem = MemoryFileSystem::new();
    let payload = vec![0x5a; 128 * 1024];
    let data_inode = filesystem
        .create(ROOT_INODE, b"payload", FileType::File, 0o644)
        .unwrap();
    filesystem.write(data_inode, 0, &payload).unwrap();
    for index in 0..10_000 {
        filesystem
            .create(
                ROOT_INODE,
                format!("entry-{index:05}").as_bytes(),
                FileType::File,
                0o644,
            )
            .unwrap();
    }

    let (lookup_time, lookups_per_second) = timed(1_000_000, || {
        black_box(filesystem.lookup(ROOT_INODE, b"entry-05000").unwrap());
    });

    let mut read_buffer = vec![0; payload.len()];
    let (read_time, reads_per_second) = timed(4_096, || {
        black_box(filesystem.read(data_inode, 0, &mut read_buffer).unwrap());
    });
    let gib_per_second = reads_per_second * f64::from(u32::try_from(read_buffer.len()).unwrap())
        / (1024.0 * 1024.0 * 1024.0);

    let start = Instant::now();
    let mut sink = CountingSink(0);
    let result = filesystem
        .read_directory(ROOT_INODE, 0, 0, &mut sink)
        .unwrap();
    let directory_time = start.elapsed();

    println!("lookup: {lookups_per_second:.0} ops/s ({lookup_time:?} for 1,000,000)");
    println!("read:   {gib_per_second:.2} GiB/s ({read_time:?} for 512 MiB)");
    println!(
        "readdir: {} entries in {:?} ({:.0} entries/s, eof={})",
        sink.0,
        directory_time,
        f64::from(sink.0) / directory_time.as_secs_f64(),
        result.eof
    );
}
