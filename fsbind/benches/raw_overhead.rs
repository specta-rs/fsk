mod support;

use fsbind::{
    Filesystem, ROOT_INODE,
    raw::{Caller, NativeRequest, Operation, Platform, RawEvent, RawEventMask},
};
use std::{hint::black_box, time::Instant};
use support::BenchmarkFilesystem;

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
        black_box(filesystem.raw_event(RawEvent {
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
        }));
    }
    black_box(filesystem.lookup(ROOT_INODE, b"entry").unwrap());
}

fn main() {
    let plain = BenchmarkFilesystem::new(0, 0, false);
    let observed_fixture = BenchmarkFilesystem::new(0, 0, true);
    let iterations = 10_000_000;
    let plain: &dyn Filesystem = black_box(&plain);
    let observed: &dyn Filesystem = black_box(&observed_fixture);

    let direct = timed(iterations, || {
        black_box(plain.lookup(ROOT_INODE, b"entry").unwrap());
    });
    let disabled_mask = plain.raw_event_mask();
    let disabled = timed(iterations, || {
        adapter_lookup(plain, disabled_mask);
    });
    let enabled_mask = observed.raw_event_mask();
    let enabled = timed(iterations, || {
        adapter_lookup(observed, enabled_mask);
    });

    println!("direct:       {direct:.0} lookups/s");
    println!(
        "raw disabled: {disabled:.0} lookups/s ({:.2}% vs direct)",
        (disabled / direct - 1.0) * 100.0
    );
    println!(
        "raw enabled:  {enabled:.0} lookups/s ({:.2}% vs direct, {} events)",
        (enabled / direct - 1.0) * 100.0,
        observed_fixture.observed_events()
    );
}
