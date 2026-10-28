/* Same-TU gate wrappers are declared before upstream robust_open/close. */
static int robust_open(const char*,int,mode_t);
static int ryeosStockClose(int);
static int ryeosStockOpeningDenied(const char*);
/* Included inside the pinned amalgamation. Ordinary VFS calls are unchanged. */
static int ryeosBoundOpen(sqlite3_vfs*,const char*,sqlite3_file*,int,int*);
static int ryeosBoundFd(sqlite3_vfs*,const char*,int,int);
static int ryeosBoundOpenFd(sqlite3_vfs*,const char*,int,int);
static int ryeosBoundFile(unixFile*);
static int ryeosBoundWritable(unixFile*);
static int ryeosBoundShm(unixFile*,unixShmNode*);
static int ryeosBoundShmOpen(unixFile*,const char*,int,int);
static int ryeosIsBound(sqlite3_vfs*);
static void *ryeosBoundClosing(unixFile*);
static void ryeosBoundClosed(void*);
static void ryeosReapKeepersLocked(void);
static int ryeosBoundShmAdopt(unixFile*,unixShmNode*);

/* Bounded read-only evidence intake; no data descriptor escapes the gate. */
SQLITE_API int ryeos_sqlite_read_unbound_regular(int,unsigned char*,int,int*);
SQLITE_API int ryeos_sqlite_read_unbound_witness(int,unsigned char*,int,int*);
SQLITE_API int ryeos_sqlite_read_unbound_regular_chunk(int,sqlite3_uint64,sqlite3_uint64,unsigned char*,int,int*);

/* Private no-SQL stopped-intake entry points; same context/release ownership. */
SQLITE_API int ryeos_sqlite_stopped_create(int,const char *const*,int,const int*,const sqlite3_uint64*,const sqlite3_uint64*,const sqlite3_uint64*,void**);
SQLITE_API int ryeos_sqlite_stopped_acquire(void*);

/* Cached Unix descriptor reuse performs osStat under Unix global. Recheck
   admission there, closing the interval after unixOpen's early sample. */
#if defined(__linux__) && SQLITE_OS_UNIX && !defined(SQLITE_OMIT_WAL)
static int ryeosAdmissionStat(const char*,struct stat*,const char*);
#undef osStat
#define osStat(path,observed) ryeosAdmissionStat((path),(observed),__func__)
#endif

/* Explicit original output close: attempted Linux close consumes even on error. */
SQLITE_API int ryeos_sqlite_close_original_output(int,sqlite3_uint64,sqlite3_uint64,int,int*,int*);

/* Genuine original Bound keeper snapshot and post-close alias-safe fsync. */
SQLITE_API int ryeos_sqlite_binding_main_identity(void*,sqlite3_uint64*,sqlite3_uint64*);
/* Borrowed descriptor number of the live main keeper. */
SQLITE_API int ryeos_sqlite_binding_main_descriptor(void*,int*);
/* Retained existing main is zero bytes with no WAL role at acquisition. */
SQLITE_API int ryeos_sqlite_binding_main_empty(void*,int*);
/* SAME live Bound main/WAL VFS handles; no descriptor open/dup/close. */
SQLITE_API int ryeos_sqlite_binding_sync_live(void*,sqlite3*,int*,int*);
SQLITE_API int ryeos_sqlite_sync_closed_main(int,int,sqlite3_uint64,sqlite3_uint64,int*,int*,int*);

/* Original shared-file mappings: private same-TU fixed prepaid ledger. */
#ifndef SQLITE_OMIT_WAL
static int ryeosNativeShmReserve(unixShmNode*,int,int,int,int*);
static void *ryeosNativeShmMap(unixShmNode*,int,int,sqlite3_int64);
static int ryeosNativeShmEnd(unixShmNode*);
static int ryeosNativeShmUnmap(unixShmNode*,void*);
#endif

/* Exact O_PATH metadata ownership close; independent of SQLite mutex lifetime. */
SQLITE_API int ryeos_sqlite_close_original_metadata_pin(int,sqlite3_uint64,sqlite3_uint64,int*,int*);

/* Trusted transferred original post-ACK finalizer; kind regular0/3, directory1/2. */
SQLITE_API int ryeos_sqlite_close_original_finalizer_file(int,sqlite3_uint64,sqlite3_uint64,int,int*,int*);

/* Process-local native guard; no inherited lock, arena or FD effect. */
SQLITE_API int ryeos_native_process_matches(void);

/* Paid opaque non-SQLite projection lease, no FD escape. */
SQLITE_API int ryeos_projection_lease_open(int,int,void**,int*);
SQLITE_API int ryeos_projection_lease_close(void*,int*,int*);
