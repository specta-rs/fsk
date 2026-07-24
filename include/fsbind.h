#ifndef FSBIND_H
#define FSBIND_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void *FSBVolumeRef;

#define FSB_ABI_VERSION 2u

enum {
    FSB_KIND_FILE = 1,
    FSB_KIND_DIRECTORY = 2,
    FSB_KIND_SYMLINK = 3,
};

enum {
    FSB_FILESYSTEM_LOCAL = 0,
    FSB_FILESYSTEM_NETWORK = 1,
};

enum {
    FSB_CACHE_NONE = 0,
    FSB_CACHE_METADATA_ONLY = 1,
    FSB_CACHE_READ = 2,
    FSB_CACHE_READ_WRITE = 3,
};

enum {
    FSB_FILE_REMOTE = 0,
    FSB_FILE_LOCAL = 1,
};

enum {
    FSB_SET_SIZE = 1u << 0,
    FSB_SET_MODE = 1u << 1,
    FSB_SET_UID = 1u << 2,
    FSB_SET_GID = 1u << 3,
    FSB_SET_ACCESSED_NS = 1u << 4,
    FSB_SET_MODIFIED_NS = 1u << 5,
};

typedef struct {
    uint64_t inode;
    uint64_t parent;
    uint64_t size;
    uint64_t allocated_size;
    uint64_t generation;
    uint64_t created_ns;
    uint64_t modified_ns;
    uint64_t accessed_ns;
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint32_t link_count;
    uint8_t kind;
    uint8_t reserved[7];
} FSBAttributes;

typedef struct {
    uint32_t valid;
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint64_t accessed_ns;
    uint64_t modified_ns;
} FSBSetAttributes;

typedef struct {
    uint32_t block_size;
    uint32_t io_size;
    uint64_t total_bytes;
    uint64_t free_bytes;
    uint64_t available_bytes;
    uint64_t files;
    uint64_t free_files;
} FSBStatFS;

typedef struct {
    uint64_t inode;
    uint64_t next_cookie;
    uint32_t name_offset;
    uint16_t name_length;
    uint8_t kind;
    uint8_t reserved;
} FSBDirEntry;

typedef struct {
    uint8_t kind;
    uint8_t lazy;
    uint8_t supports_local_files;
    uint8_t kernel_cache_policy;
    uint8_t reserved[4];
} FSBConfiguration;

typedef struct {
    uint64_t lease;
    uint64_t generation;
    size_t path_length;
    uint8_t source;
    uint8_t reserved[7];
} FSBOpenFile;

FSBVolumeRef fsb_volume_create(const uint8_t *resource,
                               size_t resource_length,
                               int32_t *error_out);
uint32_t fsb_abi_version(void);
void fsb_volume_destroy(FSBVolumeRef volume);
int32_t fsb_configuration(FSBVolumeRef volume, FSBConfiguration *output);
int32_t fsb_open_file(FSBVolumeRef volume, uint64_t inode, uint8_t writable,
                      uint8_t *path, size_t path_capacity,
                      FSBOpenFile *output);
int32_t fsb_close_file(FSBVolumeRef volume, uint64_t lease);
int32_t fsb_statfs(FSBVolumeRef volume, FSBStatFS *output);
int32_t fsb_getattr(FSBVolumeRef volume, uint64_t inode, FSBAttributes *output);
int32_t fsb_setattr(FSBVolumeRef volume, uint64_t inode,
                    const FSBSetAttributes *input, FSBAttributes *output);
int32_t fsb_lookup(FSBVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length,
                   uint64_t *inode_out, uint8_t *kind_out);
int32_t fsb_readdir(FSBVolumeRef volume, uint64_t inode,
                    uint64_t cookie, uint64_t verifier,
                    FSBDirEntry *entries, size_t entry_capacity,
                    uint8_t *names, size_t name_capacity,
                    size_t *entry_count_out, size_t *name_count_out,
                    uint64_t *next_cookie_out, uint64_t *verifier_out,
                    uint8_t *eof_out);
int32_t fsb_read(FSBVolumeRef volume, uint64_t inode, uint64_t offset,
                 uint8_t *output, size_t length, size_t *count_out);
int32_t fsb_write(FSBVolumeRef volume, uint64_t inode, uint64_t offset,
                  const uint8_t *input, size_t length, size_t *count_out);
int32_t fsb_create(FSBVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length,
                   uint8_t kind, uint32_t mode, uint64_t *inode_out);
int32_t fsb_symlink(FSBVolumeRef volume, uint64_t parent,
                    const uint8_t *name, size_t name_length,
                    const uint8_t *target, size_t target_length,
                    uint32_t mode, uint64_t *inode_out);
int32_t fsb_readlink(FSBVolumeRef volume, uint64_t inode,
                     uint8_t *output, size_t capacity, size_t *count_out);
int32_t fsb_remove(FSBVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length, uint64_t inode);
int32_t fsb_rename(FSBVolumeRef volume,
                   uint64_t source_parent,
                   const uint8_t *source_name, size_t source_name_length,
                   uint64_t destination_parent,
                   const uint8_t *destination_name,
                   size_t destination_name_length,
                   uint64_t replaced_inode);
int32_t fsb_sync(FSBVolumeRef volume);

#ifdef __cplusplus
}
#endif

#endif
