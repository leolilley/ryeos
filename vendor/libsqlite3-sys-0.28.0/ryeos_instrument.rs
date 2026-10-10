//! Deterministic feature-only transform of the reviewed maintained SQLite input.
//! Upstream source provenance remains in ryeos-upstream-source.json (7956a38f).
//! The maintained beb670be input adds the independently reviewed prospective
//! interval-one pre-opcode progress admission; this transform preserves it.
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
fn replace(source: &mut String, old: &str, new: &str) -> io::Result<()> {
    if source.matches(old).count() != 1 {
        return Err(io::Error::other(format!(
            "SQLite instrumentation anchor is not unique: {old}"
        )));
    }
    *source = source.replacen(old, new, 1);
    Ok(())
}
pub fn prepare(manifest_dir: &Path, out_dir: &Path) -> io::Result<PathBuf> {
    let bytes = fs::read(manifest_dir.join("sqlite3/sqlite3.c"))?;
    if format!("{:x}", Sha256::digest(&bytes))
        != "beb670be019aac60c8b30804dcb1c2ec7a2b9028fca7acf9aec9da664543c7ad"
    {
        return Err(io::Error::other("unexpected upstream SQLite digest"));
    }
    let mut s = String::from_utf8(bytes).map_err(io::Error::other)?;
    // Original native admission precedes any automatic SQLite initialization.
    // These definitions live in the SAME pinned translation unit below.
    s.insert_str(0, "static void *ryeosNativeMalloc(int);\nstatic void ryeosNativeFree(void*);\nstatic void *ryeosNativeRealloc(void*,int);\nstatic int ryeosNativeSize(void*);\nstatic int ryeosNativeRoundup(int);\nstatic int ryeosNativeInit(void*);\nstatic void ryeosNativeShutdown(void*);\nstatic int ryeosNativeInitGate(void);\nstatic int ryeosNativeShutdownGate(void);\nstatic int ryeosNativeOpenGate(void);\nstatic int ryeosNativeConfigGate(int);\n");
    replace(
        &mut s,
        "SQLITE_PRIVATE void sqlite3MemSetDefault(void){\n  static const sqlite3_mem_methods defaultMethods = {\n     sqlite3MemMalloc,\n     sqlite3MemFree,\n     sqlite3MemRealloc,\n     sqlite3MemSize,\n     sqlite3MemRoundup,\n     sqlite3MemInit,\n     sqlite3MemShutdown,\n     0\n  };\n  sqlite3_config(SQLITE_CONFIG_MALLOC, &defaultMethods);\n}\n\n#endif /* SQLITE_SYSTEM_MALLOC */",
        "SQLITE_PRIVATE void sqlite3MemSetDefault(void){\n  static const sqlite3_mem_methods defaultMethods = {\n     ryeosNativeMalloc, ryeosNativeFree, ryeosNativeRealloc,\n     ryeosNativeSize, ryeosNativeRoundup, ryeosNativeInit,\n     ryeosNativeShutdown, 0\n  };\n  sqlite3_config(SQLITE_CONFIG_MALLOC, &defaultMethods);\n}\n\n#endif /* SQLITE_SYSTEM_MALLOC */",
    )?;
    replace(
        &mut s,
        "SQLITE_API int sqlite3_initialize(void){",
        "SQLITE_API int sqlite3_initialize(void){\n  if(!ryeosNativeInitGate()) return SQLITE_MISUSE;",
    )?;
    replace(
        &mut s,
        "SQLITE_API int sqlite3_shutdown(void){",
        "SQLITE_API int sqlite3_shutdown(void){\n  if(!ryeosNativeShutdownGate()) return SQLITE_MISUSE;",
    )?;
    replace(
        &mut s,
        "SQLITE_API int sqlite3_config(int op, ...){",
        "SQLITE_API int sqlite3_config(int op, ...){\n  if(!ryeosNativeConfigGate(op)) return SQLITE_MISUSE;",
    )?;
    replace(
        &mut s,
        "      sqlite3GlobalConfig.m = *va_arg(ap, sqlite3_mem_methods*);",
        "      sqlite3_mem_methods *original=va_arg(ap,sqlite3_mem_methods*);\n      if(original->xMalloc!=ryeosNativeMalloc || original->xFree!=ryeosNativeFree || original->xRealloc!=ryeosNativeRealloc || original->xSize!=ryeosNativeSize || original->xRoundup!=ryeosNativeRoundup || original->xInit!=ryeosNativeInit || original->xShutdown!=ryeosNativeShutdown || original->pAppData){ rc=SQLITE_MISUSE; break; }\n      sqlite3GlobalConfig.m = *original;",
    )?;
    replace(
        &mut s,
        "static int openDatabase(\n  const char *zFilename, /* Database filename UTF-8 encoded */\n  sqlite3 **ppDb,        /* OUT: Returned database handle */\n  unsigned int flags,    /* Operational flags */\n  const char *zVfs       /* Name of the VFS to use */\n){",
        "static int openDatabase(\n  const char *zFilename, /* Database filename UTF-8 encoded */\n  sqlite3 **ppDb,        /* OUT: Returned database handle */\n  unsigned int flags,    /* Operational flags */\n  const char *zVfs       /* Name of the VFS to use */\n){\n  if(!ryeosNativeOpenGate()){ if(ppDb)*ppDb=0; return SQLITE_MISUSE; }",
    )?;
    replace(
        &mut s,
        "static void releaseInodeInfo(unixFile *pFile){",
        "static void releaseInodeInfo(unixFile *pFile){",
    )?;
    replace(
        &mut s,
        "static int robust_open(const char *z, int f, mode_t m){",
        "#include \"ryeos_binding.h\"\nstatic int ryeosRawOpen(const char *z, int f, mode_t m){",
    )?;
    replace(
        &mut s,
        "  if( osClose(h) ){",
        "  if( ryeosStockClose(h) ){ ",
    )?;
    replace(
        &mut s,
        "  memset(p, 0, sizeof(unixFile));\n\n#ifdef SQLITE_ASSERT_NO_FILES",
        "  memset(p, 0, sizeof(unixFile));\n  if(ryeosStockOpeningDenied(zName)) return SQLITE_CANTOPEN;\n\n#ifdef SQLITE_ASSERT_NO_FILES",
    )?;
    // Definitions are Unix-only. Unsupported platforms export refusal below.
    replace(
        &mut s,
        "static int unixLock(sqlite3_file *id, int eFileLock){",
        "static int unixLock(sqlite3_file *id, int eFileLock){\n  if(ryeosBoundFile((unixFile*)id)!=SQLITE_OK) return SQLITE_IOERR_LOCK;",
    )?;
    replace(
        &mut s,
        "    pUnused = findReusableFd(zName, flags);",
        "    pUnused = ryeosIsBound(pVfs) ? 0 : findReusableFd(zName, flags);",
    )?;
    replace(
        &mut s,
        "\n    fd = robust_open(zName, openFlags, openMode);",
        "\n    fd = ryeosBoundOpenFd(pVfs,zName,openFlags,openMode);",
    )?;
    replace(
        &mut s,
        "\n        fd = robust_open(zName, openFlags, openMode);",
        "\n        fd = ryeosBoundOpenFd(pVfs,zName,openFlags,openMode);",
    )?;
    replace(
        &mut s,
        "    if( openMode && (flags & (SQLITE_OPEN_WAL|SQLITE_OPEN_MAIN_JOURNAL))!=0 ){",
        "    if(ryeosBoundFd(pVfs,zName,fd,flags)!=SQLITE_OK){ rc=SQLITE_CANTOPEN; goto open_finished; }\n    if( !ryeosIsBound(pVfs) && openMode && (flags & (SQLITE_OPEN_WAL|SQLITE_OPEN_MAIN_JOURNAL))!=0 ){",
    )?;
    replace(
        &mut s,
        "        pShmNode->hShm = robust_open(zShm, O_RDWR|O_CREAT|O_NOFOLLOW,",
        "        pShmNode->hShm = ryeosBoundShmOpen(pDbFd,zShm, O_RDWR|O_CREAT|O_NOFOLLOW,",
    )?;
    replace(
        &mut s,
        "        pShmNode->hShm = robust_open(zShm, O_RDONLY|O_NOFOLLOW,",
        "        pShmNode->hShm = ryeosBoundShmOpen(pDbFd,zShm, O_RDONLY|O_NOFOLLOW,",
    )?;
    replace(
        &mut s,
        "      robustFchown(pShmNode->hShm, sStat.st_uid, sStat.st_gid);",
        "      if(ryeosBoundShmAdopt(pDbFd,pShmNode)!=SQLITE_OK){ rc=SQLITE_IOERR_SHMOPEN; goto shm_open_err; }\n      if(!ryeosIsBound(pDbFd->pVfs)) robustFchown(pShmNode->hShm, sStat.st_uid, sStat.st_gid);",
    )?;
    replace(
        &mut s,
        "  /* Make the new connection a child of the unixShmNode */",
        "  if(ryeosBoundShmAdopt(pDbFd,pShmNode)!=SQLITE_OK){ rc=SQLITE_IOERR_SHMOPEN; goto shm_open_err; }\n  /* Make the new connection a child of the unixShmNode */",
    )?;
    replace(
        &mut s,
        "    if( deleteFlag && pShmNode->hShm>=0 ){",
        "    if( deleteFlag && pShmNode->hShm>=0 && !ryeosIsBound(pDbFd->pVfs) ){",
    )?;
    replace(
        &mut s,
        "static int unixTruncate(sqlite3_file *id, i64 nByte){",
        "static int unixTruncate(sqlite3_file *id, i64 nByte){\n  if(ryeosBoundWritable((unixFile*)id)!=SQLITE_OK) return SQLITE_IOERR_TRUNCATE;",
    )?;
    replace(
        &mut s,
        "  int wrote = 0;\n  assert( id );",
        "  int wrote = 0;\n  if(ryeosBoundWritable(pFile)!=SQLITE_OK) return SQLITE_IOERR_WRITE;\n  assert( id );",
    )?;
    replace(
        &mut s,
        "  pShmNode = p->pShmNode;\n  sqlite3_mutex_enter(pShmNode->pShmMutex);",
        "  pShmNode = p->pShmNode;\n  if(ryeosBoundShm(pDbFd,pShmNode)!=SQLITE_OK) return SQLITE_IOERR_SHMMAP;\n  sqlite3_mutex_enter(pShmNode->pShmMutex);",
    )?;
    replace(
        &mut s,
        "  if( NEVER(pShmNode==0) ) return SQLITE_IOERR_SHMLOCK;\n  aLock = pShmNode->aLock;",
        "  if( NEVER(pShmNode==0) ) return SQLITE_IOERR_SHMLOCK;\n  if((flags & SQLITE_SHM_LOCK) && ryeosBoundShm(pDbFd,pShmNode)!=SQLITE_OK) return SQLITE_IOERR_SHMLOCK;\n  aLock = pShmNode->aLock;",
    )?;
    replace(
        &mut s,
        "static int unixFetch(sqlite3_file *fd, i64 iOff, int nAmt, void **pp){",
        "static int unixFetch(sqlite3_file *fd, i64 iOff, int nAmt, void **pp){\n  if(ryeosBoundFile((unixFile*)fd)!=SQLITE_OK){ *pp=0; return SQLITE_IOERR_MMAP; }",
    )?;
    replace(
        &mut s,
        "  sqlite3_vfs *pVfs;                  /* The VFS that created this unixFile */",
        "  sqlite3_vfs *pVfs;                  /* The VFS that created this unixFile */\n  int ryeosCounted;",
    )?;
    replace(
        &mut s,
        "static int closeUnixFile(sqlite3_file *id){\n  unixFile *pFile = (unixFile*)id;",
        "static int closeUnixFile(sqlite3_file *id){\n  unixFile *pFile = (unixFile*)id;\n  void *ryeosClosing=ryeosBoundClosing(pFile);",
    )?;
    replace(
        &mut s,
        "  memset(pFile, 0, sizeof(unixFile));\n  return SQLITE_OK;",
        "  memset(pFile, 0, sizeof(unixFile));\n  ryeosBoundClosed(ryeosClosing);\n  return SQLITE_OK;",
    )?;
    replace(
        &mut s,
        "static int unixFileControl(sqlite3_file *id, int op, void *pArg){\n  unixFile *pFile = (unixFile*)id;",
        "static int unixFileControl(sqlite3_file *id, int op, void *pArg){\n  unixFile *pFile = (unixFile*)id;\n  if(ryeosBoundFile(pFile)!=SQLITE_OK) return SQLITE_IOERR;\n  if(ryeosIsBound(pFile->pVfs) && op==SQLITE_FCNTL_PERSIST_WAL){ *(int*)pArg=1; return SQLITE_OK; }",
    )?;
    replace(
        &mut s,
        "      sqlite3_mutex_free(pInode->pLockMutex);\n      sqlite3_free(pInode);\n    }\n  }\n}",
        "      sqlite3_mutex_free(pInode->pLockMutex);\n      sqlite3_free(pInode);\n    }\n  }\n  ryeosReapKeepersLocked();\n}",
    )?;
    replace(
        &mut s,
        "    p->pInode->pShmNode = 0;\n    sqlite3_free(p);\n  }\n}",
        "    p->pInode->pShmNode = 0;\n    sqlite3_free(p);\n  }\n  ryeosReapKeepersLocked();\n}",
    )?;
    replace(
        &mut s,
        "  nReqRegion = ((iRegion+nShmPerMap) / nShmPerMap) * nShmPerMap;",
        "  rc = ryeosNativeShmReserve(pShmNode,iRegion,szRegion,nShmPerMap,&nReqRegion);\n  if(rc!=SQLITE_OK) goto shmpage_out;",
    )?;
    replace(
        &mut s,
        "        pMem = osMmap(0, nMap,\n            pShmNode->isReadonly ? PROT_READ : PROT_READ|PROT_WRITE,\n            MAP_SHARED, pShmNode->hShm, szRegion*(i64)pShmNode->nRegion\n        );",
        "        pMem = ryeosNativeShmMap(pShmNode,nMap,\n            pShmNode->isReadonly ? PROT_READ : PROT_READ|PROT_WRITE,\n            szRegion*(i64)pShmNode->nRegion\n        );",
    )?;
    replace(
        &mut s,
        "        osMunmap(p->apRegion[i], p->szRegion);",
        "        ryeosNativeShmUnmap(p,p->apRegion[i]);",
    )?;
    replace(
        &mut s,
        "shmpage_out:\n  if( pShmNode->nRegion>iRegion ){\n    *pp = pShmNode->apRegion[iRegion];\n  }else{\n    *pp = 0;\n  }\n  if( pShmNode->isReadonly && rc==SQLITE_OK ) rc = SQLITE_READONLY;",
        "shmpage_out:\n  { int originalEnd=ryeosNativeShmEnd(pShmNode);\n    if(rc==SQLITE_OK && originalEnd!=SQLITE_OK) rc=originalEnd; }\n  if( iRegion>=0 && pShmNode->nRegion>iRegion ){\n    *pp = pShmNode->apRegion[iRegion];\n  }else{\n    *pp = 0;\n  }\n  if( pShmNode->isReadonly && rc==SQLITE_OK ) rc = SQLITE_READONLY;",
    )?;
    s.push_str("\n#include \"ryeos_binding.c\"\n");
    let output = out_dir.join("ryeos_sqlite.c");
    fs::write(&output, s)?;
    Ok(output)
}
