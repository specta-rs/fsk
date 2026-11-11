use super::*;

fn filesystem() -> GoogleDrive {
    GoogleDrive {
        client: Client::new(),
        token: String::new(),
        nodes: vec![Node {
            parent: ROOT_INODE,
            name: Vec::new(),
            kind: FileType::Directory,
            size: 0,
            remote: None,
            writable: false,
        }],
        children: HashMap::new(),
        cache: Mutex::new(HashMap::new()),
        sizes: Mutex::new(HashMap::new()),
        writes: Mutex::new(()),
        dirty: Mutex::new(HashSet::new()),
        versions: Mutex::new(HashMap::new()),
        total_bytes: 0,
        free_bytes: 0,
    }
}

fn add_tree_with_limits(
    filesystem: &mut GoogleDrive,
    root_inode: u64,
    root_id: &str,
    files: &[DriveFile],
    max_nodes: usize,
    max_depth: usize,
) {
    let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
    let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
    let mut mounted = HashSet::new();
    filesystem.expand_tree_aliases_with(
        &mut ids,
        &mut ancestry,
        files,
        &mut mounted,
        max_nodes,
        max_depth,
    );
}

fn add_tree_with_stats(
    filesystem: &mut GoogleDrive,
    root_inode: u64,
    root_id: &str,
    files: &[DriveFile],
    max_nodes: usize,
    max_depth: usize,
) -> (usize, usize) {
    let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
    let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
    let mut mounted = HashSet::new();
    filesystem.expand_tree_aliases_with(
        &mut ids,
        &mut ancestry,
        files,
        &mut mounted,
        max_nodes,
        max_depth,
    )
}

fn file(mime_type: &str, can_modify_content: bool) -> DriveFile {
    DriveFile {
        id: "id".to_owned(),
        name: "name".to_owned(),
        mime_type: mime_type.to_owned(),
        parents: Vec::new(),
        size: 0,
        capabilities: Capabilities {
            can_modify_content,
            can_download: true,
        },
        shortcut_details: None,
        resolved_target: None,
        version: "1".to_owned(),
    }
}

#[test]
fn ordinary_files_follow_drive_write_capability() {
    assert!(drive_file_is_writable(&file("text/plain", true)));
    assert!(!drive_file_is_writable(&file("text/plain", false)));
}

#[test]
fn folders_and_exported_workspace_files_stay_read_only() {
    assert!(!drive_file_is_writable(&file(FOLDER, true)));
    assert!(!drive_file_is_writable(&file(
        "application/vnd.google-apps.document",
        true,
    )));
}

#[test]
fn parses_string_encoded_drive_file_size() {
    let file: DriveFile =
        serde_json::from_str(r#"{"id":"id","name":"file","mimeType":"text/plain","size":"42"}"#)
            .expect("Drive API file should deserialize");
    assert_eq!(file.size, 42);
}

#[test]
fn portable_names_avoid_host_special_names_and_characters() {
    assert_eq!(portable_name("a/b:c"), b"a_b_c");
    assert_eq!(portable_name("a\u{1f}b"), b"a_b");
    assert_eq!(portable_name("CON"), b"_CON");
    assert_eq!(portable_name("trailing. "), b"trailing");
    assert!(portable_name(&"x".repeat(300)).len() <= 240);
    let long_con = portable_name(&format!("CON.{}", "x".repeat(300)));
    assert!(long_con.starts_with(b"_CON"));
    assert!(long_con.len() <= 240);
    assert!(!long_con.ends_with(b" ") && !long_con.ends_with(b"."));
    // Prepending `_` makes this 241 bytes, so re-truncation exposes the
    // separator that follows `CON` and the trailing trim must remove it.
    let prefixed = portable_name(&format!("CON.{} x", " ".repeat(234)));
    assert_eq!(prefixed, b"_CON");
    assert!(!prefixed.ends_with(b" ") && !prefixed.ends_with(b"."));
    let truncated_space = portable_name(&format!("{} {}", "x".repeat(239), "y".repeat(50)));
    assert_eq!(truncated_space, "x".repeat(239).into_bytes());
    assert!(!truncated_space.ends_with(b" "));
    // Truncating a multibyte name must not split a character.
    let multibyte = portable_name(&("a".to_owned() + &"€".repeat(80)));
    assert!(multibyte.starts_with(b"a"));
    assert!(std::str::from_utf8(&multibyte).is_ok());
    assert!(multibyte.len() <= 240);
    assert_eq!(multibyte.len(), 1 + 79 * 3);
    assert_eq!(portable_name(&" ".repeat(300)), b"_");
    // Windows reserves the console/clock device aliases and the superscript
    // forms of the numbered ports.
    assert_eq!(portable_name("CONIN$"), b"_CONIN$");
    assert_eq!(portable_name("conout$"), b"_conout$");
    assert_eq!(portable_name("CLOCK$"), b"_CLOCK$");
    assert_eq!(portable_name("COM¹"), b"_COM\xC2\xB9");
    assert_eq!(portable_name("com²"), b"_com\xC2\xB2");
    assert_eq!(portable_name("LPT³"), b"_LPT\xC2\xB3");
    assert_eq!(portable_name("COM10"), b"COM10");
    // These only share a prefix with a reserved device; they must not be
    // rewritten by a loose prefix match.
    assert_eq!(portable_name("COM¹x"), b"COM\xC2\xB9x");
    assert_eq!(portable_name("CLOCK$foo"), b"CLOCK$foo");
    assert!(names_equal("É".as_bytes(), "é".as_bytes()));
    assert!(names_equal("é".as_bytes(), "e\u{301}".as_bytes()));
}

#[test]
fn collision_suffixes_stay_within_portable_name_limit() {
    let mut filesystem = filesystem();
    // A three-byte character means the 236-byte truncation target for a
    // ` (n)` suffix falls inside a character, so byte-level truncation
    // would produce invalid UTF-8.
    let long_name = "€".repeat(80);
    let inodes: Vec<_> = (0..13)
        .map(|_| filesystem.add_node(ROOT_INODE, &long_name, FileType::File, 0, None, false))
        .collect();
    let names: Vec<&[u8]> = inodes
        .iter()
        .map(|inode| {
            filesystem.nodes[inode_index(*inode).unwrap()]
                .name
                .as_slice()
        })
        .collect();

    for name in &names {
        assert!(name.len() <= 240);
        assert!(std::str::from_utf8(name).is_ok());
    }
    assert!(names[9].ends_with(b" (10)"));
    assert!(names[10].ends_with(b" (11)"));
    let unique: HashSet<&[u8]> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len());
}

#[test]
fn trees_use_actual_root_ids_and_include_shared_descendants() {
    let mut filesystem = filesystem();
    let my_drive = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );
    let mut folder = file(FOLDER, false);
    folder.id = "folder".to_owned();
    folder.parents = vec!["actual-root-id".to_owned()];
    let mut child = file("text/plain", false);
    child.id = "child".to_owned();
    child.parents = vec!["folder".to_owned()];

    filesystem.add_tree(my_drive, "actual-root-id", &[folder.clone(), child.clone()]);
    let (my_folder, _) = filesystem
        .lookup(my_drive, b"name")
        .expect("top-level folder");
    assert!(filesystem.lookup(my_folder, b"name").is_ok());

    filesystem.add_shared_tree(shared, &[folder], &[child]);
    let (shared_folder, _) = filesystem.lookup(shared, b"name").expect("shared folder");
    assert!(filesystem.lookup(shared_folder, b"name").is_ok());
}

#[test]
fn nested_direct_shares_keep_root_and_parent_aliases() {
    let mut filesystem = filesystem();
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );
    let mut folder = file(FOLDER, false);
    folder.id = "folder".to_owned();
    folder.name = "folder".to_owned();
    let mut child = file("text/plain", false);
    child.id = "child".to_owned();
    child.name = "child".to_owned();
    child.parents = vec!["folder".to_owned()];

    filesystem.add_shared_tree(shared, &[folder.clone(), child.clone()], &[folder, child]);

    let (shared_folder, _) = filesystem.lookup(shared, b"folder").expect("shared folder");
    assert!(filesystem.lookup(shared, b"child").is_ok());
    assert!(filesystem.lookup(shared_folder, b"child").is_ok());
}

#[test]
fn duplicated_direct_shares_create_one_root_alias() {
    let mut filesystem = filesystem();
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );
    let mut folder = file(FOLDER, false);
    folder.id = "folder".to_owned();
    folder.name = "folder".to_owned();
    let mut child = file("text/plain", false);
    child.id = "child".to_owned();
    child.name = "child".to_owned();
    child.parents = vec!["folder".to_owned()];

    // Pagination can overlap, listing the same share more than once.
    let roots = [folder.clone(), folder.clone(), child.clone(), child];
    filesystem.add_shared_tree(shared, &roots, &roots);

    let (folder_inode, _) = filesystem.lookup(shared, b"folder").expect("folder alias");
    assert!(filesystem.lookup(shared, b"folder (2)").is_err());
    assert!(filesystem.lookup(shared, b"child").is_ok());
    assert!(filesystem.lookup(shared, b"child (2)").is_err());
    assert!(filesystem.lookup(folder_inode, b"child").is_ok());
    assert!(filesystem.lookup(folder_inode, b"child (2)").is_err());
}

#[test]
fn first_mounts_are_not_limited_by_the_alias_cap() {
    // A flat My Drive has one first mount per file and no aliases at all.
    const FILES: usize = 50;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let files: Vec<_> = (0..FILES)
        .map(|index| {
            let mut entry = file("text/plain", false);
            entry.id = format!("id{index}");
            entry.name = format!("file{index}");
            entry.parents = vec!["root".to_owned()];
            entry
        })
        .collect();

    // The alias cap is far below the number of files, but none of these
    // mounts is an alias, so every file still lands in the tree.
    add_tree_with_limits(&mut filesystem, root, "root", &files, 4, 4);

    assert_eq!(filesystem.children[&root].len(), FILES);
    assert_eq!(filesystem.nodes.len(), 2 + FILES);
}

#[test]
fn shortcut_listed_before_its_target_is_not_an_alias() {
    // The shortcut registers its target id as a parent lookup key, but that
    // must not make the target's own first mount look like an alias.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "shortcut".to_owned();
    shortcut.name = "shortcut".to_owned();
    shortcut.parents = vec!["root".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "target".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });
    let mut target = file(FOLDER, false);
    target.id = "target".to_owned();
    target.name = "target".to_owned();
    target.parents = vec!["root".to_owned()];

    // A zero alias cap keeps only first mounts, so both entries must exist.
    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &[shortcut, target],
        0,
        MAX_ALIAS_DEPTH,
    );

    assert!(filesystem.lookup(root, b"shortcut").is_ok());
    assert!(filesystem.lookup(root, b"target").is_ok());
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 2);
}

#[test]
fn shortcut_targeting_its_parent_is_mounted_once() {
    // A shortcut whose resolved target is its own parent is still a first
    // mount. It must appear once under the parent, but it must not be
    // re-expanded under its own alias.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "folder".to_owned();
    folder.name = "folder".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "shortcut".to_owned();
    shortcut.name = "shortcut".to_owned();
    shortcut.parents = vec!["folder".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "folder".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });

    filesystem.add_tree(root, "root", &[folder, shortcut]);

    let (folder_inode, _) = filesystem.lookup(root, b"folder").expect("parent folder");
    let (shortcut_inode, _) = filesystem
        .lookup(folder_inode, b"shortcut")
        .expect("shortcut under its target parent");
    assert!(filesystem.lookup(shortcut_inode, b"shortcut").is_err());
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 2);
}

#[test]
fn distinct_shortcuts_are_first_mounts_not_aliases() {
    const SHORTCUTS: usize = 10;
    const CHILDREN: usize = 10;
    const MAX_NODES: usize = 3;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut files = vec![folder];
    for index in 0..SHORTCUTS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec!["root".to_owned()];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: "F".to_owned(),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }
    for index in 0..CHILDREN {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_NODES,
        MAX_ALIAS_DEPTH,
    );

    let (folder_inode, _) = filesystem.lookup(root, b"F").expect("canonical folder");
    // Every distinct shortcut is its own first mount, even though they all
    // target the same folder.
    for index in 0..SHORTCUTS {
        assert!(
            filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .is_ok(),
            "shortcut S{index} must be mounted"
        );
    }
    // The folder's children are first mounts under the canonical folder...
    for index in 0..CHILDREN {
        assert!(
            filesystem
                .lookup(folder_inode, format!("C{index}").as_bytes())
                .is_ok(),
            "child C{index} must be mounted under F"
        );
    }
    // ...while re-mounting those same children under the shortcuts is
    // bounded by the alias cap.
    let alias_children: usize = (0..SHORTCUTS)
        .map(|index| {
            let (shortcut_inode, _) = filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .expect("shortcut");
            filesystem.children.get(&shortcut_inode).map_or(0, Vec::len)
        })
        .sum();
    assert_eq!(alias_children, MAX_NODES);
    assert_eq!(
        filesystem.nodes.len(),
        1 + 1 + 1 + SHORTCUTS + CHILDREN + MAX_NODES
    );
}

#[test]
fn alias_expansion_stops_at_the_alias_cap() {
    // F and S are distinct listing entries, so both are first mounts. Only
    // the ten children re-mounted beneath S count as aliases.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "S".to_owned();
    shortcut.name = "S".to_owned();
    shortcut.parents = vec!["root".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "F".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });
    let mut files = vec![folder, shortcut];
    for index in 0..10 {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    add_tree_with_limits(&mut filesystem, root, "root", &files, 3, MAX_ALIAS_DEPTH);

    let (folder_inode, _) = filesystem.lookup(root, b"F").expect("canonical folder");
    let (shortcut_inode, _) = filesystem.lookup(root, b"S").expect("shortcut");
    // Every child is a first mount under the canonical folder.
    for index in 0..10 {
        assert!(
            filesystem
                .lookup(folder_inode, format!("C{index}").as_bytes())
                .is_ok(),
            "child C{index} must be mounted under F"
        );
    }
    // Only three re-mounts under the shortcut alias are allowed.
    assert_eq!(filesystem.children[&shortcut_inode].len(), 3);
    // Filesystem root, the mounted root, F, S, ten children, three aliases.
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 1 + 1 + 10 + 3);
}

#[test]
fn plain_deep_trees_are_not_truncated_by_alias_limits() {
    // The alias limits are far below this chain's depth, but every level is
    // a first mount, so the whole chain must still be created.
    const DEPTH: usize = 70;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut files = Vec::new();
    for level in 0..DEPTH {
        let mut folder = file(FOLDER, false);
        folder.id = format!("F{level}");
        folder.name.clone_from(&folder.id);
        folder.parents = vec![if level == 0 {
            "root".to_owned()
        } else {
            format!("F{}", level - 1)
        }];
        files.push(folder);
    }

    add_tree_with_limits(&mut filesystem, root, "root", &files, 4, 4);

    assert_eq!(filesystem.nodes.len(), 2 + DEPTH);
    let mut parent = root;
    for level in 0..DEPTH {
        let (inode, _) = filesystem
            .lookup(parent, format!("F{level}").as_bytes())
            .expect("every level is mounted");
        parent = inode;
    }
}

#[test]
fn shortcut_alias_expansion_is_bounded() {
    // Each folder contains both its child folder and a shortcut to that
    // child, so the unbounded walk would create roughly 2^20 aliases.
    const DEPTH: usize = 20;

    let mut filesystem = filesystem();
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );

    let mut root = file(FOLDER, false);
    root.id = "F0".to_owned();
    root.name = "F0".to_owned();
    let mut files = vec![root.clone()];
    for level in 0..DEPTH {
        let child_id = format!("F{}", level + 1);
        let mut child = file(FOLDER, false);
        child.id.clone_from(&child_id);
        child.name.clone_from(&child_id);
        child.parents = vec![format!("F{level}")];
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{}", level + 1);
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec![format!("F{level}")];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: child_id,
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(child);
        files.push(shortcut);
    }

    filesystem.add_shared_tree(shared, &[root], &files);

    // Filesystem root, "Shared with me", the directly shared root, one
    // first mount per folder and per shortcut, and exactly the alias cap's
    // worth of aliases.
    assert_eq!(filesystem.nodes.len(), 3 + 2 * DEPTH + MAX_ALIAS_NODES);
}

#[test]
fn alias_depth_is_bounded_but_real_folders_are_not() {
    // F0..F10 form one real folder chain, and Sk is a shortcut to Fk listed
    // inside F(k-1). The real chain is a first mount at every level, so the
    // alias depth limit must only truncate repeated alias paths.
    const FOLDERS: usize = 10;
    const MAX_DEPTH: usize = 5;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);

    let mut files = Vec::new();
    for index in 0..=FOLDERS {
        let mut folder = file(FOLDER, false);
        folder.id = format!("F{index}");
        folder.name.clone_from(&folder.id);
        folder.parents = vec![if index == 0 {
            "root".to_owned()
        } else {
            format!("F{}", index - 1)
        }];
        files.push(folder);
    }
    for index in 1..=FOLDERS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec![format!("F{}", index - 1)];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: format!("F{index}"),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }

    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_ALIAS_NODES,
        MAX_DEPTH,
    );

    // Every real folder survives along the canonical chain even though the
    // chain is deeper than the alias limit.
    let mut parent = root;
    for index in 0..=FOLDERS {
        let (inode, _) = filesystem
            .lookup(parent, format!("F{index}").as_bytes())
            .expect("canonical folder is a first mount");
        parent = inode;
    }

    // The shortcut chain is an alias path: S1 is a first mount under F0,
    // and S2 is re-mounted beneath it, but the next level is past the depth
    // limit.
    let (f0, _) = filesystem.lookup(root, b"F0").expect("F0");
    let (s1, _) = filesystem.lookup(f0, b"S1").expect("S1 under F0");
    let (s2_alias, _) = filesystem.lookup(s1, b"S2").expect("S2 alias under S1");
    assert!(filesystem.lookup(s2_alias, b"S3").is_err());
    assert!(filesystem.lookup(s2_alias, b"F3").is_err());

    // A deeper alias path can still be created where the parent fits below
    // the limit, but it is truncated one level later.
    let (f2_alias, _) = filesystem.lookup(s1, b"F2").expect("F2 alias under S1");
    let (s3_alias, _) = filesystem
        .lookup(f2_alias, b"S3")
        .expect("S3 alias under the F2 alias");
    assert!(filesystem.lookup(s3_alias, b"S4").is_err());
}

#[test]
fn alias_cap_keeps_the_seen_set_proportional_to_the_tree() {
    // Many aliases of one folder times many children is the case that used
    // to record every (file, alias-parent) pair even after the cap was hit.
    const SHORTCUTS: usize = 50;
    const CHILDREN: usize = 50;
    const MAX_NODES: usize = 5;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut files = vec![folder];
    for index in 0..SHORTCUTS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec!["root".to_owned()];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: "F".to_owned(),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }
    for index in 0..CHILDREN {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    let (nodes_created, seen) = add_tree_with_stats(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_NODES,
        MAX_ALIAS_DEPTH,
    );

    // One folder, every shortcut, every child, plus only the capped aliases.
    assert_eq!(nodes_created, 1 + SHORTCUTS + CHILDREN + MAX_NODES);
    // Recording every rejected (file, alias-parent) pair would take roughly
    // CHILDREN * SHORTCUTS == 2,500 entries. The early-outs keep `seen` near
    // the number of created nodes instead.
    assert!(
        seen <= nodes_created + files.len(),
        "seen={seen} nodes={nodes_created} files={}",
        files.len()
    );

    let alias_children: usize = (0..SHORTCUTS)
        .map(|index| {
            let (shortcut_inode, _) = filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .expect("shortcut");
            filesystem.children.get(&shortcut_inode).map_or(0, Vec::len)
        })
        .sum();
    assert_eq!(alias_children, MAX_NODES);
}

#[test]
fn directly_shared_chains_are_complete_regardless_of_listing_order() {
    let mut filesystem = filesystem();
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );
    let mut outer = file(FOLDER, false);
    outer.id = "outer".to_owned();
    outer.name = "outer".to_owned();
    let mut inner = file(FOLDER, false);
    inner.id = "inner".to_owned();
    inner.name = "inner".to_owned();
    inner.parents = vec!["outer".to_owned()];
    let mut leaf = file("text/plain", false);
    leaf.id = "leaf".to_owned();
    leaf.name = "leaf".to_owned();
    leaf.parents = vec!["inner".to_owned()];

    // Drive listing order is unspecified, so descendants may arrive first.
    let listed = [leaf.clone(), inner.clone(), outer.clone()];
    filesystem.add_shared_tree(shared, &listed, &listed);

    // Every directly shared file is still an alias at the root.
    let (outer_inode, _) = filesystem.lookup(shared, b"outer").expect("outer alias");
    let (inner_alias, _) = filesystem.lookup(shared, b"inner").expect("inner alias");
    let (leaf_alias, _) = filesystem.lookup(shared, b"leaf").expect("leaf alias");

    // ...and the full chain is rebuilt beneath the shared ancestors.
    let (inner_inode, _) = filesystem
        .lookup(outer_inode, b"inner")
        .expect("inner under outer");
    let (leaf_inode, _) = filesystem
        .lookup(inner_inode, b"leaf")
        .expect("leaf under outer/inner");
    assert!(filesystem.lookup(inner_alias, b"leaf").is_ok());
    assert_ne!(inner_alias, inner_inode);
    assert_ne!(leaf_alias, leaf_inode);
}

#[test]
fn cyclic_shared_listings_terminate() {
    let mut filesystem = filesystem();
    let shared = filesystem.add_node(
        ROOT_INODE,
        "Shared with me",
        FileType::Directory,
        0,
        None,
        false,
    );
    let mut left = file(FOLDER, false);
    left.id = "left".to_owned();
    left.name = "left".to_owned();
    left.parents = vec!["right".to_owned()];
    let mut right = file(FOLDER, false);
    right.id = "right".to_owned();
    right.name = "right".to_owned();
    right.parents = vec!["left".to_owned()];

    let listed = [left, right];
    filesystem.add_shared_tree(shared, &listed, &listed);

    let (left_inode, _) = filesystem.lookup(shared, b"left").expect("left alias");
    let (right_inode, _) = filesystem.lookup(shared, b"right").expect("right alias");
    assert!(filesystem.lookup(left_inode, b"right").is_ok());
    assert!(filesystem.lookup(right_inode, b"left").is_ok());
}

#[test]
fn folder_shortcuts_preserve_identity_and_duplicate_target_children() {
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "target".to_owned();
    folder.parents = vec!["root-id".to_owned()];
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "shortcut".to_owned();
    shortcut.parents = vec!["root-id".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "target".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });
    let mut child = file("text/plain", false);
    child.id = "child".to_owned();
    child.parents = vec!["target".to_owned()];

    // Deliberately list the child before the shortcut: a one-shot attach
    // would leave it missing beneath the shortcut's alias of the target.
    filesystem.add_tree(root, "root-id", &[folder, child, shortcut]);
    let (folder_inode, _) = filesystem.lookup(root, b"name").expect("real folder");
    let (shortcut_inode, _) = filesystem.lookup(root, b"name (2)").expect("shortcut");
    assert!(filesystem.lookup(folder_inode, b"name").is_ok());
    assert!(filesystem.lookup(shortcut_inode, b"name").is_ok());
}

#[test]
fn unsupported_and_download_restricted_files_are_hidden() {
    let mut unsupported = file("application/vnd.google-apps.form", true);
    assert!(!supported_file(&unsupported));
    unsupported.mime_type = "text/plain".to_owned();
    unsupported.capabilities.can_download = false;
    assert!(!supported_file(&unsupported));
}

#[test]
fn writes_buffer_by_remote_id_until_close() {
    let mut filesystem = filesystem();
    let inode = filesystem.add_node(
        ROOT_INODE,
        "file",
        FileType::File,
        3,
        Some(RemoteFile {
            id: "id".to_owned(),
            mime_type: "text/plain".to_owned(),
        }),
        true,
    );
    filesystem
        .cache
        .lock()
        .expect("cache")
        .insert("id".to_owned(), b"abc".to_vec());

    assert_eq!(filesystem.write(inode, 1, b"XY"), Ok(2));
    assert_eq!(
        filesystem.cache.lock().expect("cache").get("id"),
        Some(&b"aXY".to_vec())
    );
    assert!(filesystem.dirty.lock().expect("dirty").contains("id"));

    let cached = filesystem.cache.lock().expect("cache").clone();
    let sizes = filesystem.sizes.lock().expect("sizes").clone();
    let dirty = filesystem.dirty.lock().expect("dirty").clone();
    assert_eq!(
        filesystem.write(inode, MAX_BUFFERED_FILE_SIZE, b"x"),
        Err(Error(27))
    );
    assert_eq!(
        filesystem.set_metadata(
            inode,
            SetMetadata {
                size: Some(MAX_BUFFERED_FILE_SIZE + 1),
                ..SetMetadata::default()
            }
        ),
        Err(Error(27))
    );
    assert_eq!(*filesystem.cache.lock().expect("cache"), cached);
    assert_eq!(*filesystem.sizes.lock().expect("sizes"), sizes);
    assert_eq!(*filesystem.dirty.lock().expect("dirty"), dirty);
}

#[test]
fn completed_download_does_not_replace_concurrent_cached_write() {
    let cache = Mutex::new(HashMap::from([(
        "id".to_owned(),
        b"new dirty data".to_vec(),
    )]));
    let selected =
        cache_download(&cache, "id", b"stale download".to_vec()).expect("select cached data");
    assert_eq!(selected, b"new dirty data");
    assert_eq!(
        cache.lock().expect("cache").get("id"),
        Some(&b"new dirty data".to_vec())
    );
}

#[test]
fn stale_remote_versions_are_rejected_and_drive_quota_stays_authoritative() {
    assert!(version_matches("7", "7"));
    assert!(!version_matches("7", "8"));

    let mut filesystem = filesystem();
    filesystem.total_bytes = 2_000;
    filesystem.free_bytes = 1_000;
    filesystem
        .sizes
        .lock()
        .expect("sizes")
        .insert("id".to_owned(), 125);
    let stats = filesystem.statfs().expect("statfs");
    assert_eq!(stats.total_bytes, 2_000);
    assert_eq!(stats.free_bytes, 1_000);
    assert_eq!(stats.available_bytes, 1_000);
}

/// Mount a real Drive account through FUSE and read the CI fixture.
///
/// Ignored by default because it needs network access, Google credentials, and
/// a working FUSE installation. CI runs it with:
///
/// ```sh
/// cargo test -p fsk-google-drive -- --ignored live_mount
/// ```
///
/// Set `FSK_DRIVE_TEST_PATH` to override the fixture path under `My Drive`
/// (default: `fsk-ci/hello.txt`). Its contents must be `hello from fsk ci\n`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires network access, Google credentials, and FUSE"]
fn live_mount_reads_drive_fixture() {
    let filesystem = GoogleDrive::connect(live_access_token()).expect("connect to Google Drive");
    let mount = tempfile::tempdir().expect("mount directory");
    let session =
        fsk::MountSession::mount_network(filesystem, mount.path()).expect("mount Google Drive");

    let fixture =
        std::env::var("FSK_DRIVE_TEST_PATH").unwrap_or_else(|_| "fsk-ci/hello.txt".to_owned());
    let mut path = mount.path().join("My Drive");
    for segment in fixture.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
    }

    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(content, "hello from fsk ci\n", "unexpected fixture content");

    let metadata = std::fs::metadata(&path).expect("fixture metadata");
    assert_eq!(metadata.len(), content.len() as u64);

    let missing = std::fs::read(mount.path().join("My Drive").join("fsk-ci-missing-file"));
    assert_eq!(
        missing.expect_err("missing files must fail").kind(),
        std::io::ErrorKind::NotFound
    );

    drop(session);
}

#[cfg(target_os = "linux")]
fn live_access_token() -> String {
    if let Ok(token) = std::env::var("GOOGLE_DRIVE_ACCESS_TOKEN") {
        return token;
    }
    let client_id = std::env::var("GOOGLE_CLIENT_ID")
        .expect("GOOGLE_CLIENT_ID must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    let client_secret = std::env::var("GOOGLE_CLIENT_SECRET")
        .expect("GOOGLE_CLIENT_SECRET must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    let refresh_token = std::env::var("GOOGLE_DRIVE_REFRESH_TOKEN")
        .expect("GOOGLE_DRIVE_REFRESH_TOKEN must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    refresh_access_token(&client_id, &client_secret, &refresh_token)
        .expect("exchange Google Drive refresh token")
}
