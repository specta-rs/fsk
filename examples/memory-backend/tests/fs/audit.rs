use super::*;

#[test]
fn audit_zero_length_write_beyond_eof_is_a_noop() {
    let filesystem = MemoryFileSystem::with_capacity(BLOCK_SIZE);
    let inode = filesystem
        .create(ROOT_INODE, b"empty", FileType::File, 0o644)
        .unwrap();
    filesystem
        .set_metadata(
            inode,
            SetMetadata {
                accessed_ns: Some(1),
                modified_ns: Some(1),
                ..SetMetadata::default()
            },
        )
        .unwrap();
    let before = filesystem.metadata(inode).unwrap();
    let used_before = filesystem.statfs().unwrap().free_bytes;

    assert_eq!(filesystem.write(inode, BLOCK_SIZE, &[]), Ok(0));

    let after = filesystem.metadata(inode).unwrap();
    assert_eq!(
        after.size, before.size,
        "a zero-byte write must not extend EOF"
    );
    assert_eq!(after.allocated_size, before.allocated_size);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.modified_ns, before.modified_ns);
    assert_eq!(filesystem.statfs().unwrap().free_bytes, used_before);
}

#[test]
fn audit_successful_replace_releases_old_inode_and_capacity() {
    let filesystem = MemoryFileSystem::with_capacity(BLOCK_SIZE * 2);
    let source = filesystem
        .create(ROOT_INODE, b"source", FileType::File, 0o644)
        .unwrap();
    let destination = filesystem
        .create(ROOT_INODE, b"destination", FileType::File, 0o644)
        .unwrap();
    filesystem.write(source, 0, b"new").unwrap();
    filesystem.write(destination, 0, b"old").unwrap();
    assert_eq!(filesystem.statfs().unwrap().free_bytes, 0);

    filesystem
        .rename(
            ROOT_INODE,
            b"source",
            ROOT_INODE,
            b"destination",
            Some(destination),
        )
        .unwrap();

    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"destination"),
        Ok((source, FileType::File))
    );
    assert_eq!(filesystem.metadata(destination), Err(Error::NOT_FOUND));
    assert_eq!(filesystem.statfs().unwrap().free_bytes, BLOCK_SIZE);
    let mut output = [0; 3];
    assert_eq!(filesystem.read(source, 0, &mut output), Ok(3));
    assert_eq!(&output, b"new");
}

#[test]
fn audit_truncate_preserves_prefix_and_zero_fills_regrowth() {
    let filesystem = MemoryFileSystem::new();
    let inode = filesystem
        .create(ROOT_INODE, b"file", FileType::File, 0o640)
        .unwrap();
    filesystem.write(inode, 0, b"abcdef").unwrap();

    filesystem
        .set_metadata(
            inode,
            SetMetadata {
                size: Some(3),
                ..SetMetadata::default()
            },
        )
        .unwrap();
    let mut output = [0xff; 6];
    assert_eq!(filesystem.read(inode, 0, &mut output), Ok(3));
    assert_eq!(&output[..3], b"abc");

    filesystem
        .set_metadata(
            inode,
            SetMetadata {
                size: Some(6),
                mode: Some(0o600),
                uid: Some(123),
                gid: Some(456),
                accessed_ns: Some(7),
                modified_ns: Some(8),
            },
        )
        .unwrap();
    assert_eq!(filesystem.read(inode, 0, &mut output), Ok(6));
    assert_eq!(&output, b"abc\0\0\0");
    let metadata = filesystem.metadata(inode).unwrap();
    assert_eq!(
        (metadata.mode, metadata.uid, metadata.gid),
        (0o600, 123, 456)
    );
    assert_eq!((metadata.accessed_ns, metadata.modified_ns), (7, 8));
    assert_eq!(filesystem.synchronize(), Ok(()));
}

static AUDIT_HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn install_lookup_hook(
    filesystem: &MemoryFileSystem,
    inode: u64,
) -> (std::sync::MutexGuard<'static, ()>, Arc<AuditLookupHook>) {
    let hook_test_lock = AUDIT_HOOK_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = Arc::new(AuditLookupHook {
        reached: std::sync::Barrier::new(2),
        resume: std::sync::Barrier::new(2),
    });
    let node = filesystem.node(inode).unwrap();
    *AUDIT_LOOKUP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&node) as usize, Arc::clone(&hook)));
    (hook_test_lock, hook)
}

fn clear_lookup_hook() {
    AUDIT_LOOKUP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

fn allocated_bytes(filesystem: &MemoryFileSystem) -> u64 {
    filesystem
        .nodes
        .iter()
        .map(|entry| entry.state.read().metadata.allocated_size)
        .sum()
}

#[test]
fn audit_inflight_growth_after_unlink_does_not_leak_capacity() {
    let filesystem = Arc::new(MemoryFileSystem::with_capacity(BLOCK_SIZE * 3));
    let victim = filesystem
        .create(ROOT_INODE, b"victim", FileType::File, 0o644)
        .unwrap();
    let survivor = filesystem
        .create(ROOT_INODE, b"survivor", FileType::File, 0o644)
        .unwrap();
    filesystem.write(victim, 0, b"a").unwrap();
    filesystem.write(survivor, 0, b"b").unwrap();
    let (hook_test_lock, hook) = install_lookup_hook(&filesystem, victim);
    let writer = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || filesystem.write(victim, BLOCK_SIZE, b"x"))
    };
    hook.reached.wait();
    let (removed_tx, removed_rx) = std::sync::mpsc::channel();
    let remover = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || {
            let result = filesystem.remove(ROOT_INODE, b"victim", victim);
            removed_tx.send(result).unwrap();
        })
    };
    let _removed_before_resume = removed_rx.recv_timeout(std::time::Duration::from_millis(250));
    hook.resume.wait();
    assert_eq!(writer.join().unwrap(), Ok(1));
    remover.join().unwrap();
    clear_lookup_hook();
    drop(hook_test_lock);

    assert_eq!(allocated_bytes(&filesystem), BLOCK_SIZE);
    assert_eq!(
        filesystem.statfs().unwrap().free_bytes,
        filesystem.capacity - allocated_bytes(&filesystem),
        "an in-flight write on an unlinked inode must not remain charged"
    );
}

#[test]
fn audit_inflight_truncate_after_unlink_does_not_underflow_capacity() {
    let filesystem = Arc::new(MemoryFileSystem::with_capacity(BLOCK_SIZE * 2));
    let victim = filesystem
        .create(ROOT_INODE, b"victim", FileType::File, 0o644)
        .unwrap();
    filesystem
        .write(
            victim,
            0,
            &vec![b'a'; usize::try_from(BLOCK_SIZE * 2).unwrap()],
        )
        .unwrap();
    let (hook_test_lock, hook) = install_lookup_hook(&filesystem, victim);
    let truncater = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || {
            filesystem.set_metadata(
                victim,
                SetMetadata {
                    size: Some(0),
                    ..SetMetadata::default()
                },
            )
        })
    };
    hook.reached.wait();
    let (removed_tx, removed_rx) = std::sync::mpsc::channel();
    let remover = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || {
            let result = filesystem.remove(ROOT_INODE, b"victim", victim);
            removed_tx.send(result).unwrap();
        })
    };
    let _removed_before_resume = removed_rx.recv_timeout(std::time::Duration::from_millis(250));
    hook.resume.wait();
    truncater.join().unwrap().unwrap();
    remover.join().unwrap();
    clear_lookup_hook();
    drop(hook_test_lock);

    assert_eq!(allocated_bytes(&filesystem), 0);
    assert_eq!(
        filesystem.statfs().unwrap().free_bytes,
        filesystem.capacity,
        "an in-flight truncate on an unlinked inode must not double-free its blocks"
    );
}

#[test]
fn audit_inflight_growth_during_replace_does_not_leak_capacity() {
    let filesystem = Arc::new(MemoryFileSystem::with_capacity(BLOCK_SIZE * 4));
    let source = filesystem
        .create(ROOT_INODE, b"source", FileType::File, 0o644)
        .unwrap();
    let replaced = filesystem
        .create(ROOT_INODE, b"destination", FileType::File, 0o644)
        .unwrap();
    let survivor = filesystem
        .create(ROOT_INODE, b"survivor", FileType::File, 0o644)
        .unwrap();
    filesystem.write(source, 0, b"s").unwrap();
    filesystem.write(replaced, 0, b"r").unwrap();
    filesystem.write(survivor, 0, b"u").unwrap();
    let (hook_test_lock, hook) = install_lookup_hook(&filesystem, replaced);
    let writer = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || filesystem.write(replaced, BLOCK_SIZE, b"x"))
    };
    hook.reached.wait();
    let (renamed_tx, renamed_rx) = std::sync::mpsc::channel();
    let renamer = {
        let filesystem = Arc::clone(&filesystem);
        std::thread::spawn(move || {
            let result = filesystem.rename(
                ROOT_INODE,
                b"source",
                ROOT_INODE,
                b"destination",
                Some(replaced),
            );
            renamed_tx.send(result).unwrap();
            result
        })
    };
    let _renamed_before_resume = renamed_rx.recv_timeout(std::time::Duration::from_millis(250));
    hook.resume.wait();
    assert_eq!(writer.join().unwrap(), Ok(1));
    assert_eq!(renamer.join().unwrap(), Ok(()));
    clear_lookup_hook();
    drop(hook_test_lock);

    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"destination"),
        Ok((source, FileType::File))
    );
    assert_eq!(filesystem.metadata(replaced), Err(Error::NOT_FOUND));
    assert_eq!(allocated_bytes(&filesystem), BLOCK_SIZE * 2);
    assert_eq!(
        filesystem.statfs().unwrap().free_bytes,
        filesystem.capacity - allocated_bytes(&filesystem),
        "growth racing replacement of the same inode must be reclaimed"
    );
}

#[test]
fn audit_directory_cookie_skips_removed_slots_without_duplicates() {
    let filesystem = MemoryFileSystem::new();
    let alpha = filesystem
        .create(ROOT_INODE, b"alpha", FileType::File, 0o644)
        .unwrap();
    let beta = filesystem
        .create(ROOT_INODE, b"beta", FileType::File, 0o644)
        .unwrap();
    let gamma = filesystem
        .create(ROOT_INODE, b"gamma", FileType::File, 0o644)
        .unwrap();
    filesystem.remove(ROOT_INODE, b"beta", beta).unwrap();

    let mut cookie = 0;
    let mut verifier = 0;
    let mut names = Vec::new();
    loop {
        let mut sink = VecSink {
            capacity: 1,
            ..VecSink::default()
        };
        let result = filesystem
            .read_directory(ROOT_INODE, cookie, verifier, &mut sink)
            .unwrap();
        names.extend(sink.entries.into_iter().map(|entry| entry.0));
        cookie = result.next_cookie;
        verifier = result.verifier;
        if result.eof {
            break;
        }
        assert!(result.next_cookie > 0);
    }
    assert_eq!(names, [b"alpha".to_vec(), b"gamma".to_vec()]);
    assert_ne!(alpha, gamma);
}

#[test]
fn audit_symlink_rename_remove_keeps_indexes_and_accounting() {
    let filesystem = MemoryFileSystem::with_capacity(BLOCK_SIZE * 2);
    let target = filesystem
        .create(ROOT_INODE, b"target", FileType::File, 0o644)
        .unwrap();
    filesystem.write(target, 0, b"x").unwrap();
    let link = filesystem
        .create_symlink(ROOT_INODE, b"link", b"target", 0o777)
        .unwrap();
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"link"),
        Ok((link, FileType::Symlink))
    );
    let mut output = [0; 6];
    assert_eq!(filesystem.read_symlink(link, &mut output), Ok(6));
    assert_eq!(&output, b"target");
    assert_eq!(filesystem.statfs().unwrap().free_bytes, 0);

    filesystem
        .rename(ROOT_INODE, b"link", ROOT_INODE, b"alias", None)
        .unwrap();
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"link"),
        Err(Error::NOT_FOUND)
    );
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"alias"),
        Ok((link, FileType::Symlink))
    );
    filesystem.remove(ROOT_INODE, b"alias", link).unwrap();
    assert_eq!(filesystem.metadata(link), Err(Error::NOT_FOUND));
    assert_eq!(filesystem.statfs().unwrap().free_bytes, BLOCK_SIZE);
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"target"),
        Ok((target, FileType::File))
    );
}

#[test]
fn audit_invalid_names_do_not_mutate_the_directory() {
    let filesystem = MemoryFileSystem::new();
    let mut sink = VecSink::default();
    let before = filesystem
        .read_directory(ROOT_INODE, 0, 0, &mut sink)
        .unwrap();
    let long_name = vec![b'x'; 256];
    for name in [
        b"".as_slice(),
        b".",
        b"..",
        b"a/b",
        b"a\0b",
        long_name.as_slice(),
    ] {
        assert_eq!(
            filesystem.create(ROOT_INODE, name, FileType::File, 0o644),
            Err(Error::INVALID)
        );
    }
    let mut after = VecSink::default();
    assert_eq!(
        filesystem.read_directory(ROOT_INODE, 0, before.verifier, &mut after),
        Ok(before)
    );
    assert_eq!(filesystem.statfs().unwrap().files, 1);
}
