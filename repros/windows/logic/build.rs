use std::{env, fs, path::PathBuf};

fn main() {
    let root = env::var_os("FSK_AUDIT_SOURCE_ROOT").map_or_else(
        || {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf()
        },
        PathBuf::from,
    );
    println!("cargo:rerun-if-env-changed=FSK_AUDIT_SOURCE_ROOT");
    let windows = fs::read_to_string(root.join("fsk/src/windows.rs")).unwrap();
    let core = fs::read_to_string(root.join("fsk/src/core.rs")).unwrap();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut generated = String::from(r#"use fsk::{DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, Result, ROOT_INODE};
use std::{collections::HashMap, io, sync::Arc};

pub const S_OK: HRESULT = 0;
pub const ERROR_REPARSE_POINT_ENCOUNTERED: u32 = 4395;
pub const FILE_ATTRIBUTE_READ_ONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const WINDOWS_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;
pub type HRESULT = i32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PRJ_FILE_BASIC_INFO {
    pub IsDirectory: bool,
    pub FileSize: i64,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub FileAttributes: u32,
}

pub mod core {
    use super::{Error, Result};
"#);
    generated.push_str(&extract(&core, "fn checked_io_count").replace("pub(crate) ", "pub "));
    generated.push_str("\n}\n\npub struct Api;\n\nimpl Api {\n");
    generated.push_str(r#"    pub fn compare_names(&self, left: &[u16], right: &[u16]) -> i32 {
        for (left, right) in left.iter().copied().zip(right.iter().copied()) {
            let ordering = Self::ascii_lower(left).cmp(&Self::ascii_lower(right));
            if ordering != std::cmp::Ordering::Equal {
                return ordering as i32;
            }
        }
        (left.len() as i64 - right.len() as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    fn ascii_lower(value: u16) -> u16 {
        if (0x41..=0x5a).contains(&value) { value + 0x20 } else { value }
    }
"#);
    generated.push_str("}\n\npub struct WindowsState {\n    pub filesystem: Arc<dyn Filesystem>,\n    pub api: Api,\n}\n\nimpl WindowsState {\n");
    generated.push_str(&public_item(extract(&windows, "fn inode_for_path")));
    generated.push('\n');
    if let Some(helper) = optional(&windows, "fn inode_for_path_with") {
        generated.push_str(&public_item(helper));
        generated.push('\n');
    }
    if let Some(helper) = optional(&windows, "fn lookup_enumerated") {
        generated.push_str(&public_item(helper));
        generated.push('\n');
    }
    generated.push_str("}\n\n");
    for name in [
        "fn encode_wtf8",
        "fn decode_wtf8",
        "fn push_utf8",
        "fn hydrate",
        "fn basic_info",
        "fn windows_time",
        "fn hresult",
        "fn check_hresult",
    ] {
        generated.push_str(&public_item(extract(&windows, name)));
        generated.push_str("\n\n");
    }
    if let Some(mark) = optional(&windows, "fn check_placeholder_mark_hresult") {
        generated.push_str(&public_item(mark));
    } else {
        generated.push_str("pub fn check_placeholder_mark_hresult(value: HRESULT) -> io::Result<()> {\n    check_hresult(value)\n}\n");
    }
    fs::write(out.join("windows_helpers.rs"), generated).unwrap();
    println!("cargo:rerun-if-changed=../../../fsk/src/windows.rs");
    println!("cargo:rerun-if-changed=../../../fsk/src/core.rs");
}

fn public_item(source: String) -> String {
    let index = source.find("fn ").expect("function item");
    format!("{}pub {}", &source[..index], &source[index..])
}

fn optional(source: &str, needle: &str) -> Option<String> {
    source.find(needle).map(|_| extract(source, needle))
}

fn extract(source: &str, needle: &str) -> String {
    let found = source
        .find(needle)
        .unwrap_or_else(|| panic!("missing source item {needle}"));
    let start = source[..found].rfind('\n').map_or(0, |index| index + 1);
    let open = source[found..]
        .find('{')
        .unwrap_or_else(|| panic!("missing opening brace for {needle}"))
        + found;
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut state = State::Normal;
    let mut escaped = false;
    for (relative, byte) in bytes[open..].iter().copied().enumerate() {
        let current = byte as char;
        let next = bytes.get(open + relative + 1).copied().map(char::from);
        match state {
            State::Normal => match current {
                '/' if next == Some('/') => state = State::LineComment,
                '/' if next == Some('*') => state = State::BlockComment,
                '"' => state = State::String,
                '\'' if next.is_some_and(|value| value == '\\') => {
                    state = State::Char;
                    escaped = true;
                }
                '\'' if bytes.get(open + relative + 2) == Some(&b'\'') => {
                    state = State::Char;
                }
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return source[start..=open + relative].to_owned();
                    }
                }
                _ => {}
            },
            State::LineComment => {
                if current == '\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if current == '*' && next == Some('/') {
                    state = State::Normal;
                }
            }
            State::String => {
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == '"' {
                    state = State::Normal;
                }
            }
            State::Char => {
                if escaped {
                    escaped = false;
                    if current == '\'' {
                        state = State::Normal;
                    }
                } else if current == '\\' {
                    escaped = true;
                } else if current == '\'' {
                    state = State::Normal;
                }
            }
        }
    }
    panic!("unclosed source item {needle}");
}

#[derive(Clone, Copy)]
enum State {
    Normal,
    LineComment,
    BlockComment,
    String,
    Char,
}
