/* C ABI shared with the macOS FSKit bridge. */
#ifndef UFSI_H
#define UFSI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void *UFSIVolumeRef;

#define UFSI_ABI_VERSION 2u

enum {
    UFSI_KIND_FILE = 1,
    UFSI_KIND_DIRECTORY = 2,
    UFSI_KIND_SYMLINK = 3,
};

enum {
    UFSI_FILESYSTEM_LOCAL = 0,
    UFSI_FILESYSTEM_NETWORK = 1,
};

enum {
    UFSI_CACHE_NONE = 0,
    UFSI_CACHE_METADATA_ONLY = 1,
    UFSI_CACHE_READ = 2,
    UFSI_CACHE_READ_WRITE = 3,
};

enum {
    UFSI_FILE_REMOTE = 0,
    UFSI_FILE_LOCAL = 1,
};

enum {
    UFSI_SET_SIZE = 1u << 0,
    UFSI_SET_MODE = 1u << 1,
    UFSI_SET_UID = 1u << 2,
    UFSI_SET_GID = 1u << 3,
    UFSI_SET_ACCESSED_NS = 1u << 4,
    UFSI_SET_MODIFIED_NS = 1u << 5,
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
} UFSIAttributes;

typedef struct {
    uint32_t valid;
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint64_t accessed_ns;
    uint64_t modified_ns;
} UFSISetAttributes;

typedef struct {
    uint32_t block_size;
    uint32_t io_size;
    uint64_t total_bytes;
    uint64_t free_bytes;
    uint64_t available_bytes;
    uint64_t files;
    uint64_t free_files;
} UFSIStatFS;

typedef struct {
    uint64_t inode;
    uint64_t next_cookie;
    uint32_t name_offset;
    uint16_t name_length;
    uint8_t kind;
    uint8_t reserved;
} UFSIDirEntry;

typedef struct {
    uint8_t kind;
    uint8_t lazy;
    uint8_t supports_local_files;
    uint8_t kernel_cache_policy;
    uint8_t reserved[4];
} UFSIConfiguration;

typedef struct {
    uint64_t lease;
    uint64_t generation;
    size_t path_length;
    uint8_t source;
    uint8_t reserved[7];
} UFSIOpenFile;

UFSIVolumeRef ufsi_volume_create(const uint8_t *resource,
                               size_t resource_length,
                               int32_t *error_out);
uint32_t ufsi_abi_version(void);
void ufsi_volume_destroy(UFSIVolumeRef volume);
int32_t ufsi_configuration(UFSIVolumeRef volume, UFSIConfiguration *output);
int32_t ufsi_open_file(UFSIVolumeRef volume, uint64_t inode, uint8_t writable,
                      uint8_t *path, size_t path_capacity,
                      UFSIOpenFile *output);
int32_t ufsi_close_file(UFSIVolumeRef volume, uint64_t lease);
int32_t ufsi_statfs(UFSIVolumeRef volume, UFSIStatFS *output);
int32_t ufsi_getattr(UFSIVolumeRef volume, uint64_t inode, UFSIAttributes *output);
int32_t ufsi_setattr(UFSIVolumeRef volume, uint64_t inode,
                    const UFSISetAttributes *input, UFSIAttributes *output);
int32_t ufsi_lookup(UFSIVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length,
                   uint64_t *inode_out, uint8_t *kind_out);
int32_t ufsi_readdir(UFSIVolumeRef volume, uint64_t inode,
                    uint64_t cookie, uint64_t verifier,
                    UFSIDirEntry *entries, size_t entry_capacity,
                    uint8_t *names, size_t name_capacity,
                    size_t *entry_count_out, size_t *name_count_out,
                    uint64_t *next_cookie_out, uint64_t *verifier_out,
                    uint8_t *eof_out);
int32_t ufsi_read(UFSIVolumeRef volume, uint64_t inode, uint64_t offset,
                 uint8_t *output, size_t length, size_t *count_out);
int32_t ufsi_write(UFSIVolumeRef volume, uint64_t inode, uint64_t offset,
                  const uint8_t *input, size_t length, size_t *count_out);
int32_t ufsi_create(UFSIVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length,
                   uint8_t kind, uint32_t mode, uint64_t *inode_out);
int32_t ufsi_symlink(UFSIVolumeRef volume, uint64_t parent,
                    const uint8_t *name, size_t name_length,
                    const uint8_t *target, size_t target_length,
                    uint32_t mode, uint64_t *inode_out);
int32_t ufsi_readlink(UFSIVolumeRef volume, uint64_t inode,
                     uint8_t *output, size_t capacity, size_t *count_out);
int32_t ufsi_remove(UFSIVolumeRef volume, uint64_t parent,
                   const uint8_t *name, size_t name_length, uint64_t inode);
int32_t ufsi_rename(UFSIVolumeRef volume,
                   uint64_t source_parent,
                   const uint8_t *source_name, size_t source_name_length,
                   uint64_t destination_parent,
                   const uint8_t *destination_name,
                   size_t destination_name_length,
                   uint64_t replaced_inode);
int32_t ufsi_sync(UFSIVolumeRef volume);

#ifdef __cplusplus
}
#endif

#endif
