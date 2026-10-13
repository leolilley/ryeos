/* Lillux SQLite host backend. Same TU as byte-pinned Unix VFS. */
#if defined(__linux__) && SQLITE_OS_UNIX && !defined(SQLITE_OMIT_WAL)
#include <stdio.h>
#include <stdatomic.h>
/* Original signed native arena. Allocator, VFS and shared-memory callbacks
   allocate no host bookkeeping and never call Rust. The only call into Rust is
   the installed commit verifier, made from the commit gate while this
   connection's mutex and write lock are held; it re-enters no sqlite3_* or
   ryeos_native_* entry point other than the lock-free process check. The
   shared counter pointer remains owned by the original factory. */
#include <sys/mman.h>
#include <stdint.h>
#include <limits.h>
#include <linux/stat.h>
#include <sys/syscall.h>
#include <sys/file.h>
#define RYEOS_NATIVE_TICKETS 64
#define RYEOS_NATIVE_MAPS 64
#define RYEOS_NATIVE_CONTROL 65536ULL
#define RYEOS_NATIVE_BLOCK 256ULL
#define RYEOS_NATIVE_MAGIC 0x7279656f736e6174ULL
/* Free blocks are segregated by floor(log2(span/RYEOS_NATIVE_BLOCK)). */
#define RYEOS_NATIVE_BINS 64
/* Probes of the request's own bin, always bounded. With no fit in these probes
   a strictly larger bin serves the request if one exists, else it refuses. */
#define RYEOS_NATIVE_BIN_PROBES 8
/* ABI mirrored by repr(C) OriginalNativeBudget; limits immutable. */
typedef struct RyeosNativeBudget {
  uint64_t storageBytes,stepLimit,cleanupReserved;
  uint32_t ticketLimit,reserved;
  _Atomic uint64_t serviceSteps,cleanupSteps;
  _Atomic uint32_t issuedTickets,reservedAtomic;
} RyeosNativeBudget;
typedef struct RyeosNativeBlock {
  uint64_t magic,span,previous;
  _Atomic int state; /* 0 free, 1 live, 2 deferred free; never Drop proof */
  uint32_t reserved;
} RyeosNativeBlock;
/* Arena offsets of neighbouring free blocks in the same bin, 0 for none. They
   live in the payload of a free block, which is never handed to SQLite. */
typedef struct RyeosNativeFreeLinks {
  uint64_t next,previous;
} RyeosNativeFreeLinks;
_Static_assert(sizeof(RyeosNativeBlock)+sizeof(RyeosNativeFreeLinks)<=RYEOS_NATIVE_BLOCK,
  "free links exceed one block");
/* Fixed prepaid ledger: node is an identity token, never dereferenced after
   stock purge. Actual OS mapping custody outlives node/FD/connection disposal.
   state: 0 empty, 1 reserved/no mapping, 2 actual mapping, 3 failed disposal.
   Failure is retained and not blindly retried at a potentially reused address. */
typedef struct RyeosNativeMap {
  const void *node;
  void *address;
  uint64_t bytes;
  sqlite3_int64 offset;
  int state,unmapErrno;
} RyeosNativeMap;
typedef int (*RyeosNativeVerifier)(unsigned int);
typedef struct RyeosNativeArena {
  uint64_t magic,mappingBytes,startOffset;
  RyeosNativeBudget *budget;
  uint64_t sharedLimit,sharedBytes;
  _Atomic int sharedRefused;
  RyeosNativeMap shared[RYEOS_NATIVE_MAPS];
  pthread_mutex_t mutex;
  /* Under mutex: every state-0 block is on exactly the bin list its span
     selects, and freeMap has bit i set iff bin i is non-empty. */
  uint64_t freeBins[RYEOS_NATIVE_BINS],freeMap;
  _Atomic uint64_t liveBlocks;
  _Atomic uint32_t scopes,bindingContexts;
  _Atomic int poisoned,mutexReady,reclaimed;
  _Atomic(void*) connections[RYEOS_NATIVE_TICKETS];
  _Atomic unsigned char phases[RYEOS_NATIVE_TICKETS]; /* 0 never,1 attempted,2 enrolled,3 closed */
  /* Commit verifier; in the wipe-on-fork control page, so a forked child
     reads NULL and every commit there refuses. */
  _Atomic(RyeosNativeVerifier) verifier;
} RyeosNativeArena;
#define RYEOS_LEASE_SCAN 4096
#define RYEOS_LEASE_SERVICE (RYEOS_LEASE_SCAN+RYEOS_NATIVE_TICKETS+16)
#define RYEOS_LEASE_CLEANUP (RYEOS_NATIVE_TICKETS+16)
static int ryeosProjectionLeasesEmpty(void);
static _Atomic(RyeosNativeArena*) ryeosNativeOwner=0;
static _Atomic int ryeosNativeInstall=0;
static pthread_mutex_t ryeosNativeOwnerMutex=PTHREAD_MUTEX_INITIALIZER;
static _Thread_local RyeosNativeArena *ryeosNativeScope=0;
static _Thread_local unsigned int ryeosNativeDepth=0;
static _Thread_local int ryeosNativeCleanup=0;
static _Thread_local uint32_t ryeosNativeOpenTicket=0;
/* Set only while the commit verifier runs on this thread. Scope entry, opens
   and opcode admission refuse there, so a verifier cannot re-enter SQLite. */
static _Thread_local int ryeosNativeInGate=0;
_Static_assert(sizeof(RyeosNativeBudget)==56,"native budget ABI size differs");
_Static_assert(offsetof(RyeosNativeBudget,serviceSteps)==32,"native budget ABI offset differs");
_Static_assert(sizeof(RyeosNativeArena)<4096,"native control exceeds first page");
/* Process check never takes an inherited mutex or reads an arena header
   before the kernel-wiped control marker is checked. Readers and closing
   protect the marker address during actual parent munmap. */
static _Atomic int ryeosNativeProcessPid=0;
static _Atomic unsigned int ryeosNativeProcessReaders=0;
static _Atomic int ryeosNativeProcessClosing=0,ryeosNativeProcessGuardReady=0;
static _Atomic(RyeosNativeArena*) ryeosNativeProcessMarker=0;
SQLITE_API int ryeos_native_process_matches(void){
  int pid=(int)getpid(),expected=0,original;
  RyeosNativeArena *marker;int valid;
  original=atomic_load(&ryeosNativeProcessPid);
  if(!original){
    atomic_compare_exchange_strong(&ryeosNativeProcessPid,&expected,pid);
    original=atomic_load(&ryeosNativeProcessPid);
  }
  if(original!=pid || atomic_load(&ryeosNativeProcessClosing))return 0;
  atomic_fetch_add(&ryeosNativeProcessReaders,1);
  if(atomic_load(&ryeosNativeProcessClosing)){
    atomic_fetch_sub(&ryeosNativeProcessReaders,1);return 0;
  }
  marker=atomic_load(&ryeosNativeProcessMarker);
  valid=!marker || (atomic_load(&ryeosNativeProcessGuardReady)
    && marker->magic==RYEOS_NATIVE_MAGIC);
  atomic_fetch_sub(&ryeosNativeProcessReaders,1);return valid;
}
static int ryeosNativeCharge(RyeosNativeArena *a,uint64_t steps,int cleanup){
  if(!ryeos_native_process_matches()){return 0;}
  _Atomic uint64_t *counter; uint64_t previous,limit,next;
  if(!a || !steps || atomic_load(&a->poisoned)) return 0;
  counter=cleanup?&a->budget->cleanupSteps:&a->budget->serviceSteps;
  limit=cleanup?a->budget->cleanupReserved:a->budget->stepLimit-a->budget->cleanupReserved;
  previous=atomic_load(counter);
  /* A lost CAS only means another charger landed first. The failed exchange
     reloads previous, so every retry rechecks the limit against what is
     actually charged; only the limit (or poison above) refuses. Each loss is
     another charger's success and the limit caps those, so this terminates. */
  do{
    if(previous>limit || steps>limit-previous) return 0;
    next=previous+steps;
  }while(!atomic_compare_exchange_weak(counter,&previous,next));
  return 1;
}
/* Disposal-only charge: a lost CAS race retries, since another charger made
   progress; only a real limit (or poison) refuses. */
static int ryeosNativeChargeDisposal(RyeosNativeArena *a,uint64_t steps,int cleanup){
  if(!ryeos_native_process_matches()){return 0;}
  _Atomic uint64_t *counter; uint64_t previous,limit;
  if(!a || !steps || atomic_load(&a->poisoned)) return 0;
  counter=cleanup?&a->budget->cleanupSteps:&a->budget->serviceSteps;
  limit=cleanup?a->budget->cleanupReserved:a->budget->stepLimit-a->budget->cleanupReserved;
  previous=atomic_load(counter);
  do{
    if(previous>limit || steps>limit-previous) return 0;
  }while(!atomic_compare_exchange_weak(counter,&previous,previous+steps));
  return 1;
}
static int ryeosNativeContext(RyeosNativeArena *a){
  if(!ryeos_native_process_matches()){return 0;}
  return a && a==atomic_load(&ryeosNativeOwner) && ryeosNativeScope==a
    && ryeosNativeDepth>0 && !atomic_load(&a->poisoned);
}
static int ryeosNativeInGateActive(void){ return ryeosNativeInGate; }
/* Fails closed without an arena, outside service scope, without an enrolled
   ticket for this exact connection, or without a verifier. Runs holding only
   db->mutex and the transaction's write lock. */
static int ryeosNativeCommitGate(sqlite3 *db){
  RyeosNativeArena *a;RyeosNativeVerifier verifier;uint32_t i,n,ticket=0;int verified;
  if(!ryeos_native_process_matches() || ryeosNativeInGate || !db) return 0;
  a=atomic_load(&ryeosNativeOwner);
  if(!ryeosNativeContext(a) || ryeosNativeCleanup) return 0;
  n=a->budget->ticketLimit;
  if(n>RYEOS_NATIVE_TICKETS) return 0;
  for(i=0;i<n;i++){
    if(atomic_load(&a->connections[i])==(void*)db && atomic_load(&a->phases[i])==2){ticket=i+1;break;}
  }
  if(!ticket) return 0;
  verifier=atomic_load(&a->verifier);
  if(!verifier) return 0;
  ryeosNativeInGate=1;
  verified=verifier(ticket);
  ryeosNativeInGate=0;
  return verified==1 && ryeos_native_process_matches();
}
/* Online backup copies pages outside the commit gate; refused while any
   native arena exists. */
static int ryeosNativeBackupGate(void){ return atomic_load(&ryeosNativeOwner)==0; }
static int ryeosNativeInitGate(void){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);
  return ryeosNativeContext(a) && atomic_load(&a->mutexReady)
    && (!ryeosNativeCleanup || sqlite3GlobalConfig.isInit);
}
/* Byte totals alone never establish mapped-resource terminal. Every actual
   original ledger slot must be empty under the same allocator owner lock. */
static int ryeosNativeSharedEmpty(RyeosNativeArena *a){
  if(!ryeos_native_process_matches()){return 0;}
  int i,locked=atomic_load(&a->mutexReady);
  if(!ryeosNativeCharge(a,RYEOS_NATIVE_MAPS+1,1)) return 0;
  if(locked && pthread_mutex_trylock(&a->mutex)!=0) return 0;
  if(atomic_load(&a->sharedRefused) || a->sharedBytes!=0){
    if(locked)pthread_mutex_unlock(&a->mutex);return 0;
  }
  for(i=0;i<RYEOS_NATIVE_MAPS;i++) if(a->shared[i].state!=0){
    if(locked)pthread_mutex_unlock(&a->mutex);return 0;
  }
  if(locked)pthread_mutex_unlock(&a->mutex);return 1;
}
static int ryeosNativeShutdownGate(void){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner); uint32_t i,n;
  if(!ryeosNativeContext(a) || !ryeosNativeCleanup || atomic_load(&a->scopes)!=1
      || atomic_load(&a->bindingContexts)!=0 || !ryeosNativeSharedEmpty(a)) return 0;
  n=atomic_load(&a->budget->issuedTickets);
  if(n>a->budget->ticketLimit || n>RYEOS_NATIVE_TICKETS) return 0;
  for(i=0;i<n;i++) if(a->phases[i]!=3) return 0;
  return 1;
}
static int ryeosNativeOpenGate(void){
  if(!ryeos_native_process_matches() || ryeosNativeInGate){return 0;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner); uint32_t ticket=ryeosNativeOpenTicket;
  if(!ryeosNativeContext(a) || ryeosNativeCleanup || !ticket
      || ticket>a->budget->ticketLimit || ticket>RYEOS_NATIVE_TICKETS
      || a->phases[ticket-1]!=1 || atomic_load(&a->sharedRefused)) return 0;
  ryeosNativeOpenTicket=0; /* one armed original factory open, no stock escape */
  return ryeosNativeCharge(a,1,0);
}
static int ryeosNativeConfigGate(int op){
  if(!ryeos_native_process_matches()){return 0;}
  /* Alternative heap/page/mutex/thread/worker domains cannot bypass owner.
     Default allocator installation is checked separately at MALLOC case. */
  switch(op){
    case SQLITE_CONFIG_MALLOC:
      return ryeosNativeInitGate() && sqlite3GlobalConfig.m.xMalloc==0;
    case SQLITE_CONFIG_GETMALLOC: case SQLITE_CONFIG_GETPCACHE2:
      return ryeosNativeContext(atomic_load(&ryeosNativeOwner));
    case SQLITE_CONFIG_PCACHE2:
      /* Only the first in-scope install sets the default cache;
         the config body admits nothing but SQLite's own pcache1 methods. */
      return ryeosNativeInitGate() && sqlite3GlobalConfig.pcache2.xInit==0;
    case SQLITE_CONFIG_PCACHE: case SQLITE_CONFIG_HEAP:
    case SQLITE_CONFIG_PAGECACHE: case SQLITE_CONFIG_LOOKASIDE:
    case SQLITE_CONFIG_MUTEX: case SQLITE_CONFIG_SINGLETHREAD:
    case SQLITE_CONFIG_MULTITHREAD: case SQLITE_CONFIG_SERIALIZED:
      return 0;
    default: return ryeosNativeContext(atomic_load(&ryeosNativeOwner));
  }
}
static RyeosNativeBlock *ryeosNativeFirst(RyeosNativeArena *a){
  if(!ryeos_native_process_matches()){return 0;}
  return (RyeosNativeBlock*)((unsigned char*)a+a->startOffset);
}
static int ryeosNativeBlockValid(RyeosNativeArena *a,RyeosNativeBlock *b){
  if(!ryeos_native_process_matches()){return 0;}
  uintptr_t start=(uintptr_t)a+a->startOffset,end=(uintptr_t)a+a->mappingBytes,p=(uintptr_t)b;
  return p>=start && p<=end-sizeof(*b) && ((p-start)%RYEOS_NATIVE_BLOCK)==0
    && b->magic==RYEOS_NATIVE_MAGIC && b->span>=RYEOS_NATIVE_BLOCK
    && (b->span%RYEOS_NATIVE_BLOCK)==0 && b->span<=end-p;
}
static unsigned ryeosNativeBin(uint64_t span){
  return 63u-(unsigned)__builtin_clzll(span/RYEOS_NATIVE_BLOCK);
}
static RyeosNativeFreeLinks *ryeosNativeLinks(RyeosNativeBlock *b){
  return (RyeosNativeFreeLinks*)(b+1);
}
static uint64_t ryeosNativeOffset(RyeosNativeArena *a,RyeosNativeBlock *b){
  return (uint64_t)((unsigned char*)b-(unsigned char*)a);
}
/* A listed offset must name a valid free block; anything else poisons. */
static RyeosNativeBlock *ryeosNativeListed(RyeosNativeArena *a,uint64_t offset){
  RyeosNativeBlock *b;
  if(offset<a->startOffset || offset>=a->mappingBytes) return 0;
  b=(RyeosNativeBlock*)((unsigned char*)a+offset);
  if(!ryeosNativeBlockValid(a,b) || atomic_load(&b->state)!=0) return 0;
  return b;
}
/* Caller holds a->mutex; b is free with its final span. */
static void ryeosNativeListInsert(RyeosNativeArena *a,RyeosNativeBlock *b){
  unsigned bin=ryeosNativeBin(b->span); uint64_t head=a->freeBins[bin];
  RyeosNativeFreeLinks *links=ryeosNativeLinks(b); RyeosNativeBlock *first;
  links->previous=0; links->next=head;
  if(head){
    first=ryeosNativeListed(a,head);
    if(!first){atomic_store(&a->poisoned,1);return;}
    ryeosNativeLinks(first)->previous=ryeosNativeOffset(a,b);
  }
  a->freeBins[bin]=ryeosNativeOffset(a,b); a->freeMap|=1ULL<<bin;
}
/* Caller holds a->mutex; b is listed under its current span. Returns 0 and
   poisons on a corrupt neighbour. */
static int ryeosNativeListRemove(RyeosNativeArena *a,RyeosNativeBlock *b){
  unsigned bin=ryeosNativeBin(b->span); RyeosNativeFreeLinks *links=ryeosNativeLinks(b);
  RyeosNativeBlock *previous=0,*next=0;
  if(links->previous && !(previous=ryeosNativeListed(a,links->previous))){atomic_store(&a->poisoned,1);return 0;}
  if(links->next && !(next=ryeosNativeListed(a,links->next))){atomic_store(&a->poisoned,1);return 0;}
  if(!previous && a->freeBins[bin]!=ryeosNativeOffset(a,b)){atomic_store(&a->poisoned,1);return 0;}
  if(previous) ryeosNativeLinks(previous)->next=links->next; else a->freeBins[bin]=links->next;
  if(next) ryeosNativeLinks(next)->previous=links->previous;
  if(!a->freeBins[bin]) a->freeMap&=~(1ULL<<bin);
  links->next=0; links->previous=0; return 1;
}
/* Allocation cost is bounded by the bin probes plus one, independent of how
   many blocks are live or free: a full own-bin walk per request would make the
   paid work grow with every fragment other connections leave. A fit deeper in
   the own bin than the probes reach is refused (NOMEM) when no larger bin has
   a block, rather than searched for. */
static void *ryeosNativeMalloc(int request){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner); RyeosNativeBlock *b,*found=0,*next;
  uint64_t need,remainder,offset,larger; unsigned char *end; unsigned bin,probes=0;
  if(request<=0 || !ryeosNativeContext(a)
      || !ryeosNativeCharge(a,1,ryeosNativeCleanup)) return 0;
  need=((uint64_t)request+sizeof(*b)+RYEOS_NATIVE_BLOCK-1)&~(RYEOS_NATIVE_BLOCK-1);
  if(need>(uint64_t)INT_MAX || pthread_mutex_trylock(&a->mutex)!=0) return 0;
  end=(unsigned char*)a+a->mappingBytes;
  bin=ryeosNativeBin(need);
  /* Every block in a strictly larger bin is at least twice need's bin floor. */
  larger=bin+1<RYEOS_NATIVE_BINS?a->freeMap&~((2ULL<<bin)-1):0;
  for(offset=a->freeBins[bin];offset;offset=ryeosNativeLinks(b)->next){
    if(probes>=RYEOS_NATIVE_BIN_PROBES) break;
    if(!ryeosNativeCharge(a,1,ryeosNativeCleanup)) goto refused;
    probes++;
    if(!(b=ryeosNativeListed(a,offset))){atomic_store(&a->poisoned,1);goto refused;}
    if(b->span>=need){found=b;break;}
  }
  if(!found && larger){
    if(!ryeosNativeCharge(a,1,ryeosNativeCleanup)) goto refused;
    found=ryeosNativeListed(a,a->freeBins[__builtin_ctzll(larger)]);
    if(!found || found->span<need){atomic_store(&a->poisoned,1);goto refused;}
  }
  if(!found || !ryeosNativeListRemove(a,found)) goto refused;
  b=found; remainder=b->span-need;
  if(remainder>=RYEOS_NATIVE_BLOCK){
    next=(RyeosNativeBlock*)((unsigned char*)b+need);
    next->magic=RYEOS_NATIVE_MAGIC; next->span=remainder; next->previous=need;
    next->reserved=0; atomic_store(&next->state,0);
    b->span=need;
    if((unsigned char*)next+remainder<end)
      ((RyeosNativeBlock*)((unsigned char*)next+remainder))->previous=remainder;
    ryeosNativeListInsert(a,next);
    /* A corrupt bin head left the remainder unlisted; the arena is poisoned
       and this block is never handed out. */
    if(atomic_load(&a->poisoned)) goto refused;
  }
  atomic_store(&b->state,1); atomic_fetch_add(&a->liveBlocks,1);
  pthread_mutex_unlock(&a->mutex); return b+1;
 refused:
  pthread_mutex_unlock(&a->mutex); return 0;
}
static void ryeosNativeFreeLocked(RyeosNativeArena *a,RyeosNativeBlock *b){
  if(!ryeos_native_process_matches()){return;}
  unsigned char *end=(unsigned char*)a+a->mappingBytes; RyeosNativeBlock *next,*previous;
  atomic_store(&b->state,0); atomic_fetch_sub(&a->liveBlocks,1);
  next=(RyeosNativeBlock*)((unsigned char*)b+b->span);
  if((unsigned char*)next<end && ryeosNativeBlockValid(a,next) && atomic_load(&next->state)==0){
    if(!ryeosNativeListRemove(a,next)) return;
    b->span+=next->span; next->magic=0;
  }
  if(b->previous){
    previous=(RyeosNativeBlock*)((unsigned char*)b-b->previous);
    if(ryeosNativeBlockValid(a,previous) && atomic_load(&previous->state)==0){
      if(!ryeosNativeListRemove(a,previous)) return;
      previous->span+=b->span; b->magic=0; b=previous;
    }
  }
  next=(RyeosNativeBlock*)((unsigned char*)b+b->span);
  if((unsigned char*)next<end) next->previous=b->span;
  ryeosNativeListInsert(a,b);
}
static void ryeosNativeFree(void *pointer){
  if(!ryeos_native_process_matches()){return;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner); RyeosNativeBlock *b; int state;
  if(!pointer) return;
  if(!a){return;} /* impossible admitted pointer without retained owner */
  b=((RyeosNativeBlock*)pointer)-1;
  if(!ryeosNativeBlockValid(a,b)){atomic_store(&a->poisoned,1);return;}
  state=atomic_load(&b->state);
  if(state!=1){atomic_store(&a->poisoned,1);return;}
  if(!ryeosNativeContext(a) || !ryeosNativeCharge(a,1,ryeosNativeCleanup)
      || pthread_mutex_trylock(&a->mutex)!=0){
    /* xFree cannot return an error. Keep its real block pending; no physical
       free/zero-live credit until supervised original cleanup reconciles it. */
    atomic_store(&b->state,2);return;
  }
  ryeosNativeFreeLocked(a,b);pthread_mutex_unlock(&a->mutex);
}
static int ryeosNativeSize(void *pointer){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner); RyeosNativeBlock *b;
  if(!pointer || !a) return 0;
  b=((RyeosNativeBlock*)pointer)-1;
  if(!ryeosNativeBlockValid(a,b) || atomic_load(&b->state)!=1 || b->span-sizeof(*b)>INT_MAX) return 0;
  return (int)(b->span-sizeof(*b));
}
static int ryeosNativeRoundup(int n){
  if(!ryeos_native_process_matches()){return 0;}return n>0 && n<=INT_MAX-7?(n+7)&~7:0;}
static void *ryeosNativeRealloc(void *old,int n){
  if(!ryeos_native_process_matches()){return 0;}
  void *fresh; int size;
  if(!old) return ryeosNativeMalloc(n);
  if(n<=0){ryeosNativeFree(old);return 0;}
  size=ryeosNativeSize(old);if(!size) return 0;
  if(n<=size) return old;
  fresh=ryeosNativeMalloc(n);if(!fresh) return 0;
  memcpy(fresh,old,(size_t)size);ryeosNativeFree(old);return fresh;
}
static int ryeosNativeInit(void *unused){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}(void)unused;return ryeosNativeInitGate()?SQLITE_OK:SQLITE_MISUSE;}
static void ryeosNativeShutdown(void *unused){
  if(!ryeos_native_process_matches()){return;}(void)unused; /* supervised arena remains retained */}

SQLITE_API int ryeos_native_arena_create(const RyeosNativeBudget *budget,void **out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a;RyeosNativeBlock *first;long page;uint64_t mapping,offset,requiredCleanup;int expected=0;
  if(!out) return SQLITE_MISUSE;*out=0;
  if(!budget || budget->storageBytes<=RYEOS_NATIVE_CONTROL || budget->stepLimit<=budget->cleanupReserved
      || !budget->cleanupReserved || !budget->ticketLimit || budget->ticketLimit>RYEOS_NATIVE_TICKETS)
    return SQLITE_MISUSE;
  page=sysconf(_SC_PAGESIZE);
  if(page<=0 || ((uint64_t)page&(uint64_t)(page-1))!=0) return SQLITE_MISUSE;
  /* Fixed partition, never lend unused capacity across domains. The first
     mapped page contains this ledger; external shared mappings use the other
     half of the SAME prepaid signed storage, not additional quota. */
  mapping=((budget->storageBytes-RYEOS_NATIVE_CONTROL)/2/(uint64_t)page)*(uint64_t)page;
  if(mapping%(uint64_t)page || mapping>SIZE_MAX || mapping<((uint64_t)page+RYEOS_NATIVE_BLOCK)) return SQLITE_MISUSE;
  offset=(uint64_t)page;
  if(offset<sizeof(*a))return SQLITE_MISUSE;
  requiredCleanup=(mapping-offset)/RYEOS_NATIVE_BLOCK*2+(uint64_t)budget->ticketLimit*(5+RYEOS_LEASE_CLEANUP)
    +(uint64_t)RYEOS_NATIVE_MAPS*(RYEOS_NATIVE_MAPS+4)+64;
  if(budget->cleanupReserved<requiredCleanup) return SQLITE_MISUSE;
  if(!atomic_compare_exchange_strong(&ryeosNativeInstall,&expected,1)) return SQLITE_BUSY;
  if(atomic_load(&ryeosNativeOwner) || sqlite3GlobalConfig.isInit
      || (sqlite3GlobalConfig.m.xMalloc && sqlite3GlobalConfig.m.xMalloc!=ryeosNativeMalloc)){
    atomic_store(&ryeosNativeInstall,0);return SQLITE_MISUSE;
  }
  a=mmap(0,(size_t)mapping,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
  if(a==MAP_FAILED){atomic_store(&ryeosNativeInstall,0);return SQLITE_NOMEM;}
  /* Actual mapping immediately visible to the caller even on later failure. */
  *out=a;
  atomic_store(&ryeosNativeProcessMarker,a);
  atomic_store(&ryeosNativeProcessGuardReady,0);
  memset(a,0,sizeof(*a));a->magic=RYEOS_NATIVE_MAGIC;a->mappingBytes=mapping;
  /* This page is already charged in the original mapping. Unsupported
     kernels retain the actual mapping and refuse; never fall back. */
  if(madvise(a,(size_t)page,MADV_WIPEONFORK)!=0){
    atomic_store(&ryeosNativeOwner,a);return SQLITE_ERROR;
  }
  atomic_store(&ryeosNativeProcessGuardReady,1);
  a->startOffset=offset;a->budget=(RyeosNativeBudget*)budget;
  a->sharedLimit=budget->storageBytes-RYEOS_NATIVE_CONTROL-mapping;
  if(pthread_mutex_init(&a->mutex,0)!=0){atomic_store(&ryeosNativeOwner,a);return SQLITE_ERROR;}
  atomic_store(&a->mutexReady,1);
  first=ryeosNativeFirst(a);first->magic=RYEOS_NATIVE_MAGIC;first->span=mapping-offset;
  first->previous=0;first->reserved=0;atomic_store(&first->state,0);
  ryeosNativeListInsert(a,first);
  atomic_store(&ryeosNativeOwner,a);return SQLITE_OK;
}
SQLITE_API int ryeos_native_scope_enter(void *handle,int cleanup,uint32_t ticket){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  if(ryeosNativeInGate) return SQLITE_BUSY;
  RyeosNativeArena *a=(RyeosNativeArena*)handle;int rc=SQLITE_MISUSE;
  /* Another thread holding the owner mutex is only mid-admission, mid-census
     or mid-release: every holder runs atomics, thread-local bookkeeping and
     at most one bounded unmap, and none calls back into scope entry, so a
     blocking lock cannot self-deadlock and waits out contention instead of
     refusing a concurrent callback. SQLITE_BUSY below is reserved for facts
     of this thread (inside the commit gate, or a nested scope that does not
     match the open one) that no amount of waiting changes. */
  if(pthread_mutex_lock(&ryeosNativeOwnerMutex)!=0)return SQLITE_MISUSE;
  /* Lookup/reference admission under owner gate BEFORE dereferencing mapping;
     release cannot unmap between lookup and scopes++ even on another thread. */
  if(!a || a!=atomic_load(&ryeosNativeOwner) || atomic_load(&a->poisoned)
      || (!cleanup && (!atomic_load(&a->mutexReady) || atomic_load(&a->sharedRefused)))
      || (cleanup!=0 && cleanup!=1) || ryeosNativeDepth>=32) goto done;
  if(ryeosNativeDepth){
    if(ryeosNativeScope!=a || ryeosNativeCleanup!=cleanup || ticket){rc=SQLITE_BUSY;goto done;}
    ryeosNativeDepth++;rc=SQLITE_OK;goto done;
  }
  if(ticket){
    if(cleanup || ticket>a->budget->ticketLimit || ticket>atomic_load(&a->budget->issuedTickets)
        || a->phases[ticket-1]) goto done;
    a->phases[ticket-1]=1;
  }
  atomic_fetch_add(&a->scopes,1);ryeosNativeScope=a;ryeosNativeDepth=1;
  ryeosNativeCleanup=cleanup;ryeosNativeOpenTicket=ticket;rc=SQLITE_OK;
 done:pthread_mutex_unlock(&ryeosNativeOwnerMutex);return rc;
}
SQLITE_API int ryeos_native_scope_leave(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;
  if(!ryeosNativeDepth || ryeosNativeScope!=a) return SQLITE_MISUSE;
  if(--ryeosNativeDepth==0){ryeosNativeScope=0;ryeosNativeOpenTicket=0;ryeosNativeCleanup=0;atomic_fetch_sub(&a->scopes,1);}
  return SQLITE_OK;
}
SQLITE_API int ryeos_native_arena_ready(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;
  return ryeosNativeContext(a) && sqlite3GlobalConfig.isInit
    && sqlite3GlobalConfig.m.xMalloc==ryeosNativeMalloc?SQLITE_OK:SQLITE_MISUSE;
}
SQLITE_API int ryeos_native_connection_enroll(void *handle,sqlite3 *db,uint32_t ticket){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;uint32_t i;
  if(!ryeosNativeContext(a) || !db || !ticket || ticket>a->budget->ticketLimit
      || a->phases[ticket-1]!=1 || ryeosNativeOpenTicket) return SQLITE_MISUSE;
  for(i=0;i<a->budget->ticketLimit;i++) if(a->connections[i]==db) return SQLITE_MISUSE;
  if(db->openFlags&SQLITE_OPEN_SHAREDCACHE) return SQLITE_MISUSE;
  db->aLimit[SQLITE_LIMIT_WORKER_THREADS]=0;
  a->connections[ticket-1]=db;a->phases[ticket-1]=2;return SQLITE_OK;
}
/* Install the commit verifier for this exact arena. Cleared with the arena. */
SQLITE_API int ryeos_native_commit_verifier_install(void *handle,RyeosNativeVerifier verifier){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;int rc=SQLITE_MISUSE;
  if(!verifier) return SQLITE_MISUSE;
  if(pthread_mutex_trylock(&ryeosNativeOwnerMutex)!=0)return SQLITE_BUSY;
  if(a && a==atomic_load(&ryeosNativeOwner) && !atomic_load(&a->poisoned)){
    atomic_store(&a->verifier,verifier);rc=SQLITE_OK;
  }
  pthread_mutex_unlock(&ryeosNativeOwnerMutex);return rc;
}
SQLITE_API int ryeos_native_connection_closed(void *handle,uint32_t ticket){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;
  if(!a || a!=atomic_load(&ryeosNativeOwner) || !ticket || ticket>a->budget->ticketLimit
      || ticket>atomic_load(&a->budget->issuedTickets)
      || a->phases[ticket-1]>3) return SQLITE_MISUSE;
  /* Phase zero is a genuine issued ticket whose retained Rust acquisition
     owner refused before scope/open. Only its supervised Released path calls
     this private ABI; no counter or Drop manufactures that acknowledgement. */
  a->connections[ticket-1]=0;a->phases[ticket-1]=3;return SQLITE_OK;
}
SQLITE_API int ryeos_native_arena_reclaim(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;RyeosNativeBlock *b;unsigned char *end;
  if(!ryeosNativeContext(a) || !ryeosNativeCleanup || sqlite3GlobalConfig.isInit
      || atomic_load(&a->scopes)!=1 || !ryeosNativeSharedEmpty(a))return SQLITE_BUSY;
  if(!atomic_load(&a->mutexReady)){
    if(atomic_load(&a->liveBlocks)==0){atomic_store(&a->reclaimed,1);return SQLITE_OK;}
    return SQLITE_BUSY;
  }
  if(pthread_mutex_trylock(&a->mutex)!=0)return SQLITE_BUSY;
  end=(unsigned char*)a+a->mappingBytes;b=ryeosNativeFirst(a);
  while((unsigned char*)b<end){
    if(!ryeosNativeCharge(a,1,1)){pthread_mutex_unlock(&a->mutex);return SQLITE_BUSY;}
    if(!ryeosNativeBlockValid(a,b)){atomic_store(&a->poisoned,1);pthread_mutex_unlock(&a->mutex);return SQLITE_MISUSE;}
    if(atomic_load(&b->state)==2){
      if(!ryeosNativeCharge(a,1,1)){pthread_mutex_unlock(&a->mutex);return SQLITE_BUSY;}
      /* No coalescing during this census: stable original forward extent. */
      atomic_store(&b->state,0);atomic_fetch_sub(&a->liveBlocks,1);
      ryeosNativeListInsert(a,b);
      if(atomic_load(&a->poisoned)){pthread_mutex_unlock(&a->mutex);return SQLITE_MISUSE;}
    }
    if(atomic_load(&b->state)!=0){pthread_mutex_unlock(&a->mutex);return SQLITE_BUSY;}
    b=(RyeosNativeBlock*)((unsigned char*)b+b->span);
  }
  pthread_mutex_unlock(&a->mutex);
  if(atomic_load(&a->liveBlocks)==0){atomic_store(&a->reclaimed,1);return SQLITE_OK;}
  return SQLITE_BUSY;
}
/* Any registered VFS whose struct lies inside the mapping would be walked by
   the next initialization's unix registration after munmap. With isInit==0 no
   register/unregister can pass sqlite3_initialize, but binding unlink can run
   without init, so the walk holds the static main mutex. Mutex methods exist
   before any VFS is linked; without them no thread can be linking. */
static int ryeosNativeVfsInArena(RyeosNativeArena *a){
  if(!ryeos_native_process_matches()){return 1;}
  uintptr_t start=(uintptr_t)a,end=start+(uintptr_t)a->mappingBytes,p;
  sqlite3_mutex *mutex; sqlite3_vfs *v; int found=0;
  mutex=sqlite3GlobalConfig.mutex.xMutexAlloc ? sqlite3MutexAlloc(SQLITE_MUTEX_STATIC_MAIN) : 0;
  sqlite3_mutex_enter(mutex);
  for(v=vfsList;v && !found;v=v->pNext){
    p=(uintptr_t)v;
    if(p>=start && p<end) found=1;
  }
  sqlite3_mutex_leave(mutex);
  return found;
}
SQLITE_API int ryeos_native_arena_release(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;uint64_t mapping;uint32_t i,n;int rc=SQLITE_BUSY;
  if(pthread_mutex_trylock(&ryeosNativeOwnerMutex)!=0)return SQLITE_BUSY;
  if(!a || a!=atomic_load(&ryeosNativeOwner) || sqlite3GlobalConfig.isInit
      || atomic_load(&a->scopes) || atomic_load(&a->liveBlocks) || atomic_load(&a->poisoned)
      || !atomic_load(&a->reclaimed) || atomic_load(&a->bindingContexts)
      || !ryeosNativeSharedEmpty(a) || !ryeosProjectionLeasesEmpty()
      || ryeosNativeVfsInArena(a))goto done;
  n=atomic_load(&a->budget->issuedTickets);if(n>a->budget->ticketLimit){rc=SQLITE_MISUSE;goto done;}
  for(i=0;i<n;i++)if(a->phases[i]!=3)goto done;
  if(atomic_load(&a->mutexReady)){
    if(pthread_mutex_destroy(&a->mutex)!=0)goto done;
    atomic_store(&a->mutexReady,0); /* committed before munmap/late failure */
  }
  mapping=a->mappingBytes;
  atomic_store(&a->verifier,(RyeosNativeVerifier)0);
  atomic_store(&ryeosNativeProcessClosing,1);
  if(atomic_load(&ryeosNativeProcessReaders)){
    atomic_store(&ryeosNativeProcessClosing,0);goto done;
  }
  if(munmap(a,(size_t)mapping)!=0){
    atomic_store(&ryeosNativeProcessClosing,0);rc=SQLITE_IOERR;goto done;
  }
  atomic_store(&ryeosNativeProcessMarker,0);
  atomic_store(&ryeosNativeProcessGuardReady,0);
  atomic_store(&ryeosNativeOwner,0);atomic_store(&ryeosNativeInstall,0);
  atomic_store(&ryeosNativeProcessClosing,0);rc=SQLITE_OK;
 done:pthread_mutex_unlock(&ryeosNativeOwnerMutex);return rc;
}
/* Read-only shared-mapping ledger census for tests: recorded bytes and
   nonempty slots. No effect; order owner mutex -> a->mutex as in release. */
SQLITE_API int ryeos_native_shared_census(void *handle,uint64_t *bytes,uint32_t *slots){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=(RyeosNativeArena*)handle;int i,rc=SQLITE_MISUSE;uint32_t used=0;
  if(!bytes || !slots)return SQLITE_MISUSE;
  if(pthread_mutex_trylock(&ryeosNativeOwnerMutex)!=0)return SQLITE_BUSY;
  if(!a || a!=atomic_load(&ryeosNativeOwner) || !atomic_load(&a->mutexReady))goto done;
  if(pthread_mutex_lock(&a->mutex)!=0){rc=SQLITE_BUSY;goto done;}
  for(i=0;i<RYEOS_NATIVE_MAPS;i++) if(a->shared[i].state!=0) used++;
  *bytes=a->sharedBytes;*slots=used;
  pthread_mutex_unlock(&a->mutex);rc=SQLITE_OK;
 done:pthread_mutex_unlock(&ryeosNativeOwnerMutex);return rc;
}
SQLITE_API int ryeos_native_layout(uint64_t storage,unsigned int tickets,uint64_t *cleanup){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  long page=sysconf(_SC_PAGESIZE);uint64_t mapping;
  if(!cleanup || page<=0 || (uint64_t)page<sizeof(RyeosNativeArena)
      || ((uint64_t)page&(uint64_t)(page-1)) || storage<=RYEOS_NATIVE_CONTROL
      || !tickets || tickets>RYEOS_NATIVE_TICKETS)return SQLITE_MISUSE;
  mapping=((storage-RYEOS_NATIVE_CONTROL)/2/(uint64_t)page)*(uint64_t)page;
  if(mapping%(uint64_t)page || mapping<(uint64_t)page+RYEOS_NATIVE_BLOCK || mapping>SIZE_MAX)return SQLITE_MISUSE;
  *cleanup=(mapping-(uint64_t)page)/RYEOS_NATIVE_BLOCK*2+(uint64_t)tickets*(5+RYEOS_LEASE_CLEANUP)
    +(uint64_t)RYEOS_NATIVE_MAPS*(RYEOS_NATIVE_MAPS+4)+64;
  return SQLITE_OK;
}

/* Reserve all prospective grouped maps before file-size probing/extension.
   Each operation is fixed-size and charges its entire bounded ledger scan
   before the first effect. Same-node reuse is charged once, not per SQL user. */
static int ryeosNativeShmReserve(unixShmNode *node,int region,int size,int group,int *required){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);
  uint64_t req,bytes,count,available=0;int i,page;
  if(!required || !ryeosNativeContext(a) || ryeosNativeCleanup
      || atomic_load(&a->sharedRefused) || !node || region<0 || size<=0 || group<=0
      || node->nRegion<0 || node->nRegion%group || node->hShm<0) return SQLITE_IOERR_SHMMAP;
  page=osGetpagesize();
  if(page<=0 || ((unsigned int)page&((unsigned int)page-1))) return SQLITE_IOERR_SHMSIZE;
  /* Checks precede upstream int arithmetic, pointer-array sizing and writes.
     No exclusive-lock heap fallback is silently substituted. */
  req=(((uint64_t)region+(uint64_t)group)/(uint64_t)group)*(uint64_t)group;
  if(req>INT_MAX || req>(uint64_t)INT_MAX/(uint64_t)size
      || req>(uint64_t)INT_MAX/sizeof(char*)
      || (uint64_t)size*(uint64_t)group>INT_MAX
      || size!=32768 || group!=unixShmRegionPerMap()
      || ((uint64_t)size*(uint64_t)group)%(uint64_t)page
      || (node->nRegion && node->szRegion!=size)) return SQLITE_IOERR_SHMSIZE;
  *required=(int)req;
  if(req<=(uint64_t)node->nRegion) return SQLITE_OK;
  count=(req-(uint64_t)node->nRegion)/(uint64_t)group;
  bytes=count*(uint64_t)size*(uint64_t)group;
  if(count>RYEOS_NATIVE_MAPS || !ryeosNativeCharge(a,RYEOS_NATIVE_MAPS+1,0)
      || pthread_mutex_trylock(&a->mutex)!=0) return SQLITE_IOERR_SHMMAP;
  for(i=0;i<RYEOS_NATIVE_MAPS;i++){
    if(a->shared[i].state==0) available++;
    /* A stale/overlapping reservation is never a fresh admission. */
    if(a->shared[i].node==node && (a->shared[i].state==1 || a->shared[i].state==3)){
      pthread_mutex_unlock(&a->mutex);return SQLITE_IOERR_SHMMAP;
    }
  }
  if(count>available || a->sharedBytes>a->sharedLimit || bytes>a->sharedLimit-a->sharedBytes){
    pthread_mutex_unlock(&a->mutex);return SQLITE_IOERR_SHMMAP;
  }
  for(i=0;i<RYEOS_NATIVE_MAPS && count;i++) if(a->shared[i].state==0){
    RyeosNativeMap *m=&a->shared[i];
    m->node=node;m->address=0;m->bytes=(uint64_t)size*(uint64_t)group;
    m->offset=(sqlite3_int64)((req-count*(uint64_t)group)*(uint64_t)size);
    m->unmapErrno=0;m->state=1;a->sharedBytes+=m->bytes;count--;
  }
  pthread_mutex_unlock(&a->mutex);return SQLITE_OK;
}
static void *ryeosNativeShmMap(unixShmNode *node,int bytes,int protection,sqlite3_int64 offset){
  if(!ryeos_native_process_matches()){errno=EPERM;return MAP_FAILED;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);void *address;int i;
  if(!ryeosNativeContext(a) || ryeosNativeCleanup || atomic_load(&a->sharedRefused)
      || !ryeosNativeCharge(a,RYEOS_NATIVE_MAPS+1,0)
      || pthread_mutex_trylock(&a->mutex)!=0){errno=EACCES;return MAP_FAILED;}
  for(i=0;i<RYEOS_NATIVE_MAPS;i++){
    RyeosNativeMap *m=&a->shared[i];
    if(m->state!=1 || m->node!=node || m->bytes!=(uint64_t)bytes || m->offset!=offset) continue;
    address=osMmap(0,bytes,protection,MAP_SHARED,node->hShm,offset);
    if(address!=MAP_FAILED){
      /* Actual acquisition is recorded before return, no fallible owner insert. */
      m->address=address;m->state=2;
    }
    pthread_mutex_unlock(&a->mutex);return address;
  }
  pthread_mutex_unlock(&a->mutex);errno=EACCES;return MAP_FAILED;
}
static int ryeosNativeShmEnd(unixShmNode *node){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);int i;
  if(!ryeosNativeContext(a) || !ryeosNativeCharge(a,RYEOS_NATIVE_MAPS+1,ryeosNativeCleanup)
      || pthread_mutex_trylock(&a->mutex)!=0) return SQLITE_IOERR_SHMMAP;
  for(i=0;i<RYEOS_NATIVE_MAPS;i++){
    RyeosNativeMap *m=&a->shared[i];
    if(m->node==node && m->state==1){
      /* This reservation acquired no mapping. File extension may still have
         occurred: clearing it is NOT a filesystem/no-effect terminal. */
      a->sharedBytes-=m->bytes;memset(m,0,sizeof(*m));
    }
  }
  pthread_mutex_unlock(&a->mutex);return SQLITE_OK;
}
/* Invariant: disposal of a recorded mapping does not depend on scope entry.
   Only the purge of the node's last user calls this, so a WAL connection
   closed outside every scope reaches it too. The arena cannot be released
   meanwhile: the node and its inode are live arena blocks, so liveBlocks is
   nonzero and arena_release refuses. The charge is the one an in-scope close
   would make (cleanup counter only in a cleanup scope); a lost CAS race is
   retried and only an exhausted budget refuses. The ledger lock blocks:
   purge runs under the unix big lock, and no a->mutex holder ever takes a
   SQLite or unix mutex, so the order big lock -> a->mutex has no inverse.
   Outside a scope only scope entry is waived; an unrecorded mapping or a
   refused charge still refuses, as in scope. */
static int ryeosNativeShmUnmap(unixShmNode *node,void *address){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);int i,rc,saved;
  if(!a || atomic_load(&a->poisoned)
      || (ryeosNativeDepth && !ryeosNativeContext(a))
      || !ryeosNativeChargeDisposal(a,RYEOS_NATIVE_MAPS+1,ryeosNativeCleanup)
      || pthread_mutex_lock(&a->mutex)!=0){
    if(a) atomic_store(&a->sharedRefused,1);
    return SQLITE_IOERR_SHMMAP;
  }
  for(i=0;i<RYEOS_NATIVE_MAPS;i++){
    RyeosNativeMap *m=&a->shared[i];
    if(m->node!=node || m->address!=address || m->state!=2) continue;
    rc=osMunmap(address,(size_t)m->bytes);saved=errno;
    if(rc==0){
      /* Complete full grouped disposal is committed BEFORE capacity reuse. */
      a->sharedBytes-=m->bytes;memset(m,0,sizeof(*m));
    }else{
      m->unmapErrno=saved;m->state=3;atomic_store(&a->sharedRefused,1);
    }
    pthread_mutex_unlock(&a->mutex);return rc==0?SQLITE_OK:SQLITE_IOERR_SHMMAP;
  }
  atomic_store(&a->sharedRefused,1);pthread_mutex_unlock(&a->mutex);
  return SQLITE_IOERR_SHMMAP;
}

typedef struct RyeosBinding {
  sqlite3_vfs vfs; /* first: unique registered nondefault Unix VFS */
  RyeosNativeArena *nativeArena; /* genuine allocation scope retained until actual context destruction */
  _Atomic int root;
  _Atomic int files[3]; /* main, WAL, SHM: retained host descriptors */
  _Atomic int pins[3]; /* O_PATH acquisition before effect-bearing open */
  char *names[3];
  char *paths[3];
  char *vfsName;
  _Atomic int poisoned;
  _Atomic int activeFiles;
  _Atomic int mainClaimed;
  _Atomic int failureFd;
  int stoppedIntake, stoppedPhysical, stoppedAcquired, stoppedCount;
  char stoppedNames[100][160];
  int stoppedSelected[3];
  sqlite3_uint64 stoppedDev[3], stoppedIno[3], stoppedLength[3];
  struct RyeosGateFd *stoppedNodes[3]; /* preallocated, until linked */
  int createdMain;
  int readonlyMain;
  int readonlySource; /* stable original preview; copied inspection is separate */
  struct RyeosBinding *retireNext;
  _Atomic int retirePhase; /* 1 captured, 2 barriers, 3 final release */
  _Atomic int trackedNodes;
  int barrierMain,barrierShm;
  char retirementNames[3][2][128];
  void *copyLease; /* opaque Rust owner, never destroyed under Unix mutex */
  struct RyeosBinding *pendingNext;
  int queued;
  int vfsLinked; /* set only by successful register; cleared only by unlink */
  int callerHeld;
  int retentionReason;
  _Atomic int closeErrno;
  sqlite3 *abandonedDb; /* actual busy-close fallback owner, never auto-contact */
} RyeosBinding;
static void ryeosNativeBindingRegister(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  /* malloc above succeeded only within this genuine native scope; its arena
     cannot be unmapped while this scope/context or any real heap block lives. */
  c->nativeArena=ryeosNativeScope;
  assert(c->nativeArena);
  atomic_fetch_add(&c->nativeArena->bindingContexts,1);
}
static void ryeosNativeBindingDestroyed(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  assert(c->nativeArena);
  atomic_fetch_sub(&c->nativeArena->bindingContexts,1);
  c->nativeArena=0;
}
/* Same-TU stock and bound descriptor gate. Nodes allocate BEFORE acquisition;
   rejected/failed data descriptors can transfer without close-time allocation.
   This mutex never takes Unix global or an inode mutex: stock close may already
   hold either. Retirement establishes claims in Unix-global -> gate order. */
#include <pthread.h>
typedef struct RyeosGateFd {
  struct RyeosGateFd *next;
  int fd, pin, closing;
  dev_t dev; ino_t ino;
  RyeosBinding *owner;
  _Atomic int *slot;
} RyeosGateFd;
static pthread_mutex_t ryeosGateMutex = PTHREAD_MUTEX_INITIALIZER;
static RyeosGateFd *ryeosGateFiles = 0;
typedef struct RyeosProjectionLease {
  struct RyeosProjectionLease *next;
  RyeosNativeArena *arena;
  int pin,fd,listed,closeErrno,fdConsumed,pinConsumed,locked;
  dev_t dev;ino_t ino;
} RyeosProjectionLease;
_Static_assert(sizeof(RyeosProjectionLease)+sizeof(RyeosNativeBlock)<=RYEOS_NATIVE_BLOCK,
  "projection lease exceeds one prepaid block");
static RyeosProjectionLease *ryeosProjectionLeases=0;
static int ryeosProjectionLeasesEmpty(void){
  if(!ryeos_native_process_matches())return 0;
  int empty;
  if(pthread_mutex_trylock(&ryeosGateMutex)!=0)return 0;
  empty=ryeosProjectionLeases==0;
  pthread_mutex_unlock(&ryeosGateMutex);return empty;
}
/* Lease census of one identity. A listed lease is answered only inside a
   scope of the installed arena. `charge` prepays the bounded scan from that
   arena's budget; an uncharged census runs the same identity checks under
   the same RYEOS_NATIVE_TICKETS bound and refuses exactly as the charged one. */
static int ryeosLeaseBlocksIdentityCharged(dev_t dev,ino_t ino,int charge){
  if(!ryeos_native_process_matches())return 1;
  RyeosProjectionLease *l;struct stat st;unsigned n=0;
  if(ryeosProjectionLeases){
    RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);
    if(!ryeosNativeContext(a))return 1;
    if(charge && !ryeosNativeCharge(a,RYEOS_NATIVE_TICKETS,ryeosNativeCleanup))return 1;
  }
  for(l=ryeosProjectionLeases;l;l=l->next){
    if(++n>RYEOS_NATIVE_TICKETS)return 1;
    if(l->dev==dev && l->ino==ino)return 1;
    if(l->fd>=0){
      if(osFstat(l->fd,&st)!=0)return 1;
      if(st.st_dev==dev && st.st_ino==ino)return 1;
    }
  }
  return 0;
}
static int ryeosLeaseBlocksIdentity(dev_t dev,ino_t ino){
  return ryeosLeaseBlocksIdentityCharged(dev,ino,1);
}
static int ryeosLeaseBlocksFdCharged(int fd,int charge){
  if(!ryeos_native_process_matches())return 1;
  struct stat st;
  return osFstat(fd,&st)!=0 || ryeosLeaseBlocksIdentityCharged(st.st_dev,st.st_ino,charge);
}
static int ryeosLeaseBlocksFd(int fd){
  return ryeosLeaseBlocksFdCharged(fd,1);
}
static RyeosBinding *ryeosRetiring = 0;
/* All effect-bearing open/reuse admissions are serialized by gate; Unix
   inode reuse is additionally serialized by Unix global. No unlocked gap
   between open and tracked-node registration is permitted. */
static int ryeosGateOpening = 0;
static int ryeosGateRawOpen(const char *path,int flags,mode_t mode){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  int fd; ryeosGateOpening++; fd=ryeosRawOpen(path,flags,mode); ryeosGateOpening--; return fd;
}
static void ryeosFreeStoppedNodes(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  int i; for(i=0;i<3;i++) if(c->stoppedNodes[i]){ sqlite3_free(c->stoppedNodes[i]); c->stoppedNodes[i]=0; }
}
static int ryeosGateMatches(RyeosBinding *c,int fd){
  if(!ryeos_native_process_matches()){return 0;}
  int i;
  for(i=0;i<3;i++){
    int expected=c->pins[i]>=0?c->pins[i]:c->files[i];
    if(expected>=0){
      struct stat left,right;
      if(osFstat(expected,&left)!=0 || osFstat(fd,&right)!=0) return 2;
      if(left.st_dev==right.st_dev && left.st_ino==right.st_ino) return 1;
    }
  }
  return c->closeErrno?2:0;
}
static RyeosBinding *ryeosGateClaim(int fd,RyeosBinding *exempt){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosBinding *c;
  for(c=ryeosRetiring;c;c=c->retireNext){
    if(c==exempt && c->retirePhase==3) continue;
    /* Pending intake denies admission, but MUST permit incumbent close. */
    if(c->stoppedIntake && !c->stoppedPhysical) continue;
    if(ryeosGateMatches(c,fd)) return c;
  }
  return 0;
}
static RyeosBinding *ryeosGateAdmissionClaim(int fd){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosBinding *c; struct stat actual; int i;
  if(osFstat(fd,&actual)!=0) return ryeosRetiring;
  for(c=ryeosRetiring;c;c=c->retireNext){
    if(ryeosGateMatches(c,fd)) return c;
    if(c->stoppedIntake) for(i=0;i<3;i++) if(c->stoppedSelected[i]>=0
        && (sqlite3_uint64)actual.st_dev==c->stoppedDev[i]
        && (sqlite3_uint64)actual.st_ino==c->stoppedIno[i]) return c;
  }
  return 0;
}
static int ryeosGateNamespaceBlockedLocked(int root,const char *name){
  if(!ryeos_native_process_matches()){return 1;}
  RyeosBinding *c; struct stat actual,expected; int i,j,blocked=0;
  if(osFstat(root,&actual)!=0){ blocked=1; goto done; }
  for(c=ryeosRetiring;c;c=c->retireNext){
    if(osFstat(c->root,&expected)!=0){ blocked=1; break; }
    if(actual.st_dev!=expected.st_dev || actual.st_ino!=expected.st_ino) continue;
    if(c->stoppedIntake) for(i=0;i<c->stoppedCount;i++)
      if(strcmp(name,c->stoppedNames[i])==0) blocked=1;
    for(i=0;i<3;i++){
      if(c->names[i] && strcmp(name,c->names[i])==0) blocked=1;
      for(j=0;j<2;j++) if(strcmp(name,c->retirementNames[i][j])==0) blocked=1;
    }
    if(blocked) break;
  }
done:
  return blocked;
}
static int ryeosGateNamespaceBlocked(int root,const char *name){
  if(!ryeos_native_process_matches()){return 1;}
  int result;
  pthread_mutex_lock(&ryeosGateMutex);
  result=ryeosGateNamespaceBlockedLocked(root,name);
  pthread_mutex_unlock(&ryeosGateMutex);
  return result;
}
static int ryeosGatePathBlockedLocked(const char *path){
  if(!ryeos_native_process_matches()){return 1;}
  char *copy,*slash; int root,result=0;
  if(!ryeosRetiring || !path) return 0;
  copy=sqlite3_mprintf("%s",path); if(!copy) return 1;
  slash=strrchr(copy,'/');
  if(slash){
    *slash=0;
    root=open(*copy?copy:"/",O_PATH|O_DIRECTORY|O_CLOEXEC);
    if(root<0) result=1;
    else { result=ryeosGateNamespaceBlockedLocked(root,slash+1); close(root); }
  }else result=1; /* Unix full-path inputs expected; uncertainty refuses */
  sqlite3_free(copy); return result;
}
static int ryeosStockOpeningDenied(const char *path){
  if(!ryeos_native_process_matches()){return 1;}
  RyeosBinding *c; struct stat observed,expected; int i,result=0;
  pthread_mutex_lock(&ryeosGateMutex);
  if(ryeosGatePathBlockedLocked(path)){ result=1; goto done; }
  if(path && ((int(*)(const char*,struct stat*))aSyscall[4].pCurrent)(path,&observed)==0){
    if(ryeosLeaseBlocksIdentity(observed.st_dev,observed.st_ino)){result=1;goto done;}
    for(c=ryeosRetiring;c;c=c->retireNext){
      if(c->stoppedIntake) for(i=0;i<3;i++) if(c->stoppedSelected[i]>=0
          && (sqlite3_uint64)observed.st_dev==c->stoppedDev[i]
          && (sqlite3_uint64)observed.st_ino==c->stoppedIno[i]){ result=1; goto done; }
      for(i=0;i<3;i++){
        int pin=c->pins[i]>=0?c->pins[i]:c->files[i];
        if(pin>=0 && osFstat(pin,&expected)==0 && observed.st_dev==expected.st_dev
            && observed.st_ino==expected.st_ino){ result=1; goto done; }
      }
    }
  }
done:
  pthread_mutex_unlock(&ryeosGateMutex);
  return result; /* sampled evidence can REFUSE only; effect opens re-pin */
}
/* Preserve the original configurable Unix stat syscall. While a claim is
   active, this metadata admission refusal prevents cached FD reuse; it does
   not close/release any peer descriptor. Called global -> gate by cache. */
static int ryeosAdmissionStat(const char *path,struct stat *observed,const char *caller){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  /* Byte-pinned callsite discriminator: only cache admission is guarded.
     Temp directory, xAccess, fileHasMoved and other Unix metadata semantics
     use the exact configured syscall, including relative paths. */
  if(strcmp(caller,"findReusableFd")!=0)
    return ((int(*)(const char*,struct stat*))aSyscall[4].pCurrent)(path,observed);
  RyeosBinding *c; int rc,i,denied=0; struct stat expected;
  pthread_mutex_lock(&ryeosGateMutex);
  rc=((int(*)(const char*,struct stat*))aSyscall[4].pCurrent)(path,observed);
  if(ryeosGatePathBlockedLocked(path)) denied=1;
  if(rc==0 && ryeosLeaseBlocksIdentity(observed->st_dev,observed->st_ino))denied=1;
  if(rc==0) for(c=ryeosRetiring;c;c=c->retireNext){
    for(i=0;i<3;i++){
      int pin=c->pins[i]>=0?c->pins[i]:c->files[i];
      if((pin>=0 && (osFstat(pin,&expected)!=0 || (expected.st_dev==observed->st_dev && expected.st_ino==observed->st_ino)))
          || (c->stoppedIntake && c->stoppedSelected[i]>=0
              && c->stoppedDev[i]==(sqlite3_uint64)observed->st_dev && c->stoppedIno[i]==(sqlite3_uint64)observed->st_ino)) denied=1;
    }
  }
  pthread_mutex_unlock(&ryeosGateMutex);
  if(denied){ errno=EACCES; return -1; }
  return rc;
}
static void ryeosGateForget(RyeosGateFd **link){
  if(!ryeos_native_process_matches()){return;}
  RyeosGateFd *n=*link; int error=0;
  struct stat current;
  if(osFstat(n->fd,&current)!=0 || current.st_dev!=n->dev || current.st_ino!=n->ino){
    /* An outside close/reuse violates confinement. Never close that numeric FD. */
    if(n->owner) n->owner->closeErrno=EBADF;
    return;
  }
  if(close(n->fd)!=0) error=errno;
  if(n->pin>=0 && close(n->pin)!=0 && !error) error=errno;
  if(n->slot) *n->slot=-1;
  if(n->owner){
    if(error) n->owner->closeErrno=error;
    n->owner->trackedNodes--;
  }
  *link=n->next; sqlite3_free(n);
}
static void ryeosGateFlushLocked(void){
  if(!ryeos_native_process_matches()){return;}
  RyeosGateFd **link=&ryeosGateFiles;
  while(*link){
    RyeosGateFd *n=*link;
    if(n->closing && !ryeosGateClaim(n->fd,n->owner)){
      RyeosGateFd *before=*link;
      ryeosGateForget(link);
      if(*link!=before) continue;
    }
    link=&(*link)->next;
  }
}
static void ryeosGateOwner(int fd,RyeosBinding *owner){
  if(!ryeos_native_process_matches()){return;}
  RyeosGateFd *n;
  pthread_mutex_lock(&ryeosGateMutex);
  for(n=ryeosGateFiles;n;n=n->next) if(n->fd==fd){
    if(!n->owner){ n->owner=owner; owner->trackedNodes++; }
    break;
  }
  pthread_mutex_unlock(&ryeosGateMutex);
}
static int ryeosGateClose(int fd,RyeosBinding *exempt,_Atomic int *slot){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  RyeosGateFd **link; int result=0;
  pthread_mutex_lock(&ryeosGateMutex);
  for(link=&ryeosGateFiles;*link && (*link)->fd!=fd;link=&(*link)->next){}
  if(*link){
    RyeosGateFd *node=*link;
    if(slot) node->slot=slot;
    node->closing=1;
    if(ryeosGateClaim(fd,exempt)) result=1;
    else{
      int beforeError=node->owner?node->owner->closeErrno:0;
      RyeosBinding *owner=node->owner;
      ryeosGateForget(link);
      if(*link==node) result=1;
      else if(owner && owner->closeErrno!=beforeError) result=-1;
    }
  }else if(ryeosGateClaim(fd,exempt)) result=1;
  else if(close(fd)!=0) result=-1;
  if(slot && result!=1) *slot=-1;
  ryeosGateFlushLocked();
  pthread_mutex_unlock(&ryeosGateMutex);
  return result; /* 1 retains numeric FD; 0/-1 means Linux released coordinate */
}
static int ryeosStockClose(int fd){
  if(!ryeos_native_process_matches()){return 1;} return ryeosGateClose(fd,0,0)<0; }
static int ryeosGateCreateAt(RyeosBinding *owner,const char *name){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  RyeosGateFd *node=sqlite3_malloc64(sizeof(*node));
  int fd; struct stat created; char pinned[64];
  if(!node){ errno=ENOMEM; return -1; }
  memset(node,0,sizeof(*node)); node->fd=-1; node->pin=-1;
  pthread_mutex_lock(&ryeosGateMutex);
  if(ryeosGateNamespaceBlockedLocked(owner->root,name)){
    pthread_mutex_unlock(&ryeosGateMutex); sqlite3_free(node); errno=EACCES; return -1;
  }
  ryeosGateOpening++;
  fd=openat(owner->root,name,O_RDWR|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0600);
  ryeosGateOpening--;
  if(fd<0){ pthread_mutex_unlock(&ryeosGateMutex); sqlite3_free(node); return -1; }
  node->fd=fd; node->owner=owner; owner->trackedNodes++;
  if(osFstat(fd,&created)!=0){ owner->closeErrno=errno; }
  else { node->dev=created.st_dev; node->ino=created.st_ino; }
  snprintf(pinned,sizeof(pinned),"/proc/self/fd/%d",fd);
  node->pin=open(pinned,O_PATH|O_CLOEXEC);
  /* Even pin failure leaves the acquired FD reachable in the context. */
  node->next=ryeosGateFiles; ryeosGateFiles=node;
  pthread_mutex_unlock(&ryeosGateMutex);
  return fd;
}
static int robust_open(const char *path,int flags,mode_t mode){
  RyeosGateFd *node=sqlite3_malloc64(sizeof(*node));
  int fd=-1,attempt; struct stat regular;
  if(!node){ errno=ENOMEM; return -1; }
  memset(node,0,sizeof(*node)); node->fd=-1; node->pin=-1;
  pthread_mutex_lock(&ryeosGateMutex);
  if(ryeosGatePathBlockedLocked(path)){
    pthread_mutex_unlock(&ryeosGateMutex); sqlite3_free(node); errno=EACCES; return -1;
  }
  for(attempt=0;attempt<2;attempt++){
    int pin=open(path,O_PATH|O_CLOEXEC|(flags&O_NOFOLLOW)); /* enforce ORIGINAL leaf policy */
    if(pin>=0){
      if(ryeosLeaseBlocksFd(pin)){
        close(pin);pthread_mutex_unlock(&ryeosGateMutex);sqlite3_free(node);errno=EACCES;return -1;
      }
      if(ryeosGateAdmissionClaim(pin)){
        /* O_PATH keeper enters the retiring owner's reachable fixed list;
           no protected probe close before retirement physical release. */
        RyeosBinding *claim=ryeosGateAdmissionClaim(pin);
        if(claim->stoppedIntake && !claim->stoppedPhysical){
          close(pin); pthread_mutex_unlock(&ryeosGateMutex); sqlite3_free(node); errno=EACCES; return -1;
        }
        node->fd=pin; node->pin=-1; node->closing=1;
        if(osFstat(pin,&regular)!=0){ claim->closeErrno=EBADF; }
        else { node->dev=regular.st_dev; node->ino=regular.st_ino; }
        node->owner=claim; claim->trackedNodes++;
        node->next=ryeosGateFiles; ryeosGateFiles=node;
        pthread_mutex_unlock(&ryeosGateMutex); errno=EACCES; return -1;
      }
      if(osFstat(pin,&regular)!=0){ close(pin); break; }
      if((flags&O_NOFOLLOW) && S_ISLNK(regular.st_mode)){ close(pin); errno=ELOOP; break; }
      if(flags&O_EXCL){ close(pin); errno=EEXIST; break; }
      if(S_ISREG(regular.st_mode)){
        char pinned[64]; snprintf(pinned,sizeof(pinned),"/proc/self/fd/%d",pin);
        fd=ryeosGateRawOpen(pinned,flags&~(O_CREAT|O_EXCL|O_TRUNC|O_NOFOLLOW),mode);
        if(fd>=0){ node->pin=pin; break; }
      }else fd=ryeosGateRawOpen(path,flags,mode);
      close(pin); break;
    }
    if(errno!=ENOENT) break;
    if(!(flags&O_CREAT)){ errno=ENOENT; break; }
    /* Never effectfully adopt an occupant racing an absent-path probe. */
    fd=ryeosGateRawOpen(path,flags|O_EXCL,mode);
    if(fd>=0 || errno!=EEXIST || (flags&O_EXCL)) break;
  }
  if(fd>=0 && osFstat(fd,&regular)==0 && S_ISREG(regular.st_mode)){
    node->fd=fd; node->dev=regular.st_dev; node->ino=regular.st_ino;
    node->next=ryeosGateFiles; ryeosGateFiles=node;
  }else{
    if(node->pin>=0) close(node->pin);
    sqlite3_free(node);
  }
  pthread_mutex_unlock(&ryeosGateMutex);
  return fd;
}

SQLITE_API int ryeos_projection_lease_open(int pin,int mode,void **out,int *actualFd){
  if(!ryeos_native_process_matches())return SQLITE_MISUSE;
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);RyeosProjectionLease *l,*peer;
  RyeosGateFd *node;struct stat expected,actual;int flags,rc=SQLITE_BUSY;unsigned count=0;char exact[64];
#if defined(SYS_statx) && defined(STATX_MNT_ID)
  struct statx pinMount,fdMount;
#endif
  if(!out || !actualFd)return SQLITE_MISUSE;
  *out=0;*actualFd=-1;
  if(!ryeosNativeContext(a) || ryeosNativeCleanup || !ryeosNativeOpenTicket
      || (mode!=1 && mode!=2) || !ryeosNativeCharge(a,RYEOS_LEASE_SERVICE,0))return SQLITE_MISUSE;
  l=sqlite3_malloc64(sizeof(*l));if(!l)return SQLITE_NOMEM;
  memset(l,0,sizeof(*l));l->arena=a;l->pin=pin;l->fd=-1;*out=l;
  flags=fcntl(pin,F_GETFL);
  if(flags<0 || !(flags&O_PATH) || osFstat(pin,&expected)!=0 || !S_ISREG(expected.st_mode))return SQLITE_MISUSE;
  l->dev=expected.st_dev;l->ino=expected.st_ino;
  if(pthread_mutex_trylock(&ryeosGateMutex)!=0)return SQLITE_BUSY;
  for(node=ryeosGateFiles;node;node=node->next){
    if(++count>RYEOS_LEASE_SCAN || osFstat(node->fd,&actual)!=0)goto done;
    if(actual.st_dev==l->dev && actual.st_ino==l->ino)goto done;
  }
  count=0;for(peer=ryeosProjectionLeases;peer;peer=peer->next)if(++count>=RYEOS_NATIVE_TICKETS)goto done;
  if(ryeosLeaseBlocksFd(pin) || ryeosGateAdmissionClaim(pin))goto done;
  l->next=ryeosProjectionLeases;ryeosProjectionLeases=l;l->listed=1;
  if(snprintf(exact,sizeof(exact),"/proc/self/fd/%d",pin)>=(int)sizeof(exact)){rc=SQLITE_CANTOPEN;goto done;}
  l->fd=ryeosGateRawOpen(exact,O_RDWR|O_CLOEXEC|O_NONBLOCK,0);
  *actualFd=l->fd; /* private actual ownership fact before every late refusal */
  if(l->fd<0){rc=SQLITE_CANTOPEN;goto done;}
  if(osFstat(l->fd,&actual)!=0 || !S_ISREG(actual.st_mode)
      || actual.st_dev!=l->dev || actual.st_ino!=l->ino){rc=SQLITE_IOERR;goto done;}
#if defined(SYS_statx) && defined(STATX_MNT_ID)
  memset(&pinMount,0,sizeof(pinMount));memset(&fdMount,0,sizeof(fdMount));
  if(syscall(SYS_statx,pin,"",AT_EMPTY_PATH|AT_SYMLINK_NOFOLLOW,STATX_MNT_ID,&pinMount)!=0
      || syscall(SYS_statx,l->fd,"",AT_EMPTY_PATH|AT_SYMLINK_NOFOLLOW,STATX_MNT_ID,&fdMount)!=0
      || !(pinMount.stx_mask&STATX_MNT_ID) || !(fdMount.stx_mask&STATX_MNT_ID)
      || pinMount.stx_mnt_id!=fdMount.stx_mnt_id){rc=SQLITE_IOERR;goto done;}
#else
  rc=SQLITE_NOTFOUND;goto done;
#endif
  rc=flock(l->fd,(mode==1?LOCK_SH:LOCK_EX)|LOCK_NB)==0?SQLITE_OK:SQLITE_BUSY;
  if(rc==SQLITE_OK)l->locked=mode; /* actual lock fact before host late checks */
done:
  pthread_mutex_unlock(&ryeosGateMutex);return rc;
}
SQLITE_API int ryeos_projection_lease_close(void *handle,int *pinConsumed,int *fdConsumed){
  if(!ryeos_native_process_matches())return SQLITE_MISUSE;
  RyeosProjectionLease *l=(RyeosProjectionLease*)handle,**link;struct stat st;int error=0;
  if(!pinConsumed || !fdConsumed)return SQLITE_MISUSE;
  *pinConsumed=0;*fdConsumed=0;
  if(!l || !ryeosNativeContext(l->arena) || !ryeosNativeCleanup
      || !ryeosNativeCharge(l->arena,RYEOS_LEASE_CLEANUP,1))return SQLITE_MISUSE;
  if(pthread_mutex_trylock(&ryeosGateMutex)!=0)return SQLITE_BUSY;
  *fdConsumed=l->fdConsumed;*pinConsumed=l->pinConsumed;
  if(l->closeErrno){*pinConsumed=l->pinConsumed;pthread_mutex_unlock(&ryeosGateMutex);return SQLITE_IOERR_CLOSE;}
  if(l->fd>=0){
    if(osFstat(l->fd,&st)!=0 || st.st_dev!=l->dev || st.st_ino!=l->ino){
      pthread_mutex_unlock(&ryeosGateMutex);return SQLITE_IOERR;
    }
    if(close(l->fd)!=0)error=errno;
    l->fd=-1;l->fdConsumed=1;*fdConsumed=1;
  }
  if(!l->pinConsumed){
    if(osFstat(l->pin,&st)!=0 || !S_ISREG(st.st_mode)
        || st.st_dev!=l->dev || st.st_ino!=l->ino){
      if(error)l->closeErrno=error;
      pthread_mutex_unlock(&ryeosGateMutex);return SQLITE_IOERR;
    }
    if(close(l->pin)!=0 && !error)error=errno;
    l->pinConsumed=1;*pinConsumed=1;
  }
  if(error){l->closeErrno=error;pthread_mutex_unlock(&ryeosGateMutex);return SQLITE_IOERR_CLOSE;}
  if(l->listed){
    for(link=&ryeosProjectionLeases;*link && *link!=l;link=&(*link)->next){}
    if(*link!=l){pthread_mutex_unlock(&ryeosGateMutex);return SQLITE_MISUSE;}
    *link=l->next;l->listed=0;
  }
  pthread_mutex_unlock(&ryeosGateMutex);sqlite3_free(l);return SQLITE_OK;
}

/* The O_PATH pin comes from the caller's descriptor-rooted namespace. Refuse
   every tracked SQLite inode (including stock cached peers), plus pending and
   physical retirement claims, BEFORE opening a data descriptor. Holding gate
   until that descriptor is closed prevents a new SQLite adoption in between.
   Never take Unix global, run SQL/signature code, or return an FD here. */
static int ryeosReadUnboundRegular(int pin,unsigned char *buffer,int capacity,int *written,int charge){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat expected,actual; RyeosGateFd *node;
  int fd=-1,rc=SQLITE_IOERR_READ,flags,total=0; char exact[64];
  if(!written) return SQLITE_MISUSE;
  *written=0;
  if(!buffer || capacity<2 || capacity>262145) return SQLITE_MISUSE;
  flags=fcntl(pin,F_GETFL);
  if(flags<0 || !(flags&O_PATH) || osFstat(pin,&expected)!=0 || !S_ISREG(expected.st_mode)) return SQLITE_IOERR_READ;
  if(expected.st_size<0 || expected.st_size>=capacity) return SQLITE_TOOBIG;
  pthread_mutex_lock(&ryeosGateMutex);
  for(node=ryeosGateFiles;node;node=node->next){
    if(osFstat(node->fd,&actual)!=0) goto done;
    if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino){ rc=SQLITE_BUSY; goto done; }
  }
  if(ryeosLeaseBlocksFdCharged(pin,charge) || ryeosGateAdmissionClaim(pin)){ rc=SQLITE_BUSY; goto done; }
  if(snprintf(exact,sizeof(exact),"/proc/self/fd/%d",pin)>=(int)sizeof(exact)) goto done;
  fd=ryeosGateRawOpen(exact,O_RDONLY|O_CLOEXEC|O_NONBLOCK,0);
  if(fd<0) goto done;
  if(osFstat(fd,&actual)!=0 || !S_ISREG(actual.st_mode)
      || actual.st_dev!=expected.st_dev || actual.st_ino!=expected.st_ino) goto done;
  while(total<capacity){
    ssize_t count=pread(fd,buffer+total,(size_t)(capacity-total),(off_t)total);
    /* Errors, including EINTR, refuse; no unbounded retry under gate. */
    if(count<0) goto done;
    if(count==0) break;
    total+=(int)count;
  }
  if(total==capacity){ rc=SQLITE_TOOBIG; goto done; }
  if(osFstat(fd,&actual)!=0 || actual.st_size!=total || actual.st_size!=expected.st_size){ rc=SQLITE_IOERR_READ; goto done; }
  *written=total; rc=SQLITE_OK;
done:
  if(fd>=0 && close(fd)!=0){ rc=SQLITE_IOERR_CLOSE; *written=0; }
  /* Linux consumes the descriptor even on late close/EINTR: never retry. */
  pthread_mutex_unlock(&ryeosGateMutex);
  return rc;
}
SQLITE_API int ryeos_sqlite_read_unbound_regular(int pin,unsigned char *buffer,int capacity,int *written){
  return ryeosReadUnboundRegular(pin,buffer,capacity,written,1);
}
/* Revalidation read of a retained original witness. The lease census is the
   same identity check as the charged read, without the arena charge: the
   reread runs from inside the owner's own checkpoints, whose count no
   cleanup reservation can bound. */
SQLITE_API int ryeos_sqlite_read_unbound_witness(int pin,unsigned char *buffer,int capacity,int *written){
  return ryeosReadUnboundRegular(pin,buffer,capacity,written,0);
}

/* Bounded copy chunk from the SAME retained O_PATH pin. Expected length is
   caller's whole-file snapshot, not a per-chunk allocation allowance. */
SQLITE_API int ryeos_sqlite_read_unbound_regular_chunk(int pin,sqlite3_uint64 offset,sqlite3_uint64 length,unsigned char *buffer,int capacity,int *written){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat expected,actual; RyeosGateFd *node;
  int fd=-1,rc=SQLITE_IOERR_READ,flags,total=0,wanted; char exact[64];
  if(!written) return SQLITE_MISUSE;
  *written=0;
  if(!buffer || capacity<1 || capacity>65536 || length>(sqlite3_uint64)LARGEST_INT64 || offset>length) return SQLITE_MISUSE;
  flags=fcntl(pin,F_GETFL);
  if(flags<0 || !(flags&O_PATH) || osFstat(pin,&expected)!=0 || !S_ISREG(expected.st_mode)) return SQLITE_IOERR_READ;
  if(expected.st_size<0 || (sqlite3_uint64)expected.st_size!=length) return SQLITE_IOERR_READ;
  wanted=(int)((length-offset)<(sqlite3_uint64)capacity ? length-offset : (sqlite3_uint64)capacity);
  pthread_mutex_lock(&ryeosGateMutex);
  for(node=ryeosGateFiles;node;node=node->next){
    if(osFstat(node->fd,&actual)!=0) goto done;
    if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino){ rc=SQLITE_BUSY; goto done; }
  }
  if(ryeosLeaseBlocksFd(pin) || ryeosGateAdmissionClaim(pin)){ rc=SQLITE_BUSY; goto done; }
  if(snprintf(exact,sizeof(exact),"/proc/self/fd/%d",pin)>=(int)sizeof(exact)) goto done;
  fd=ryeosGateRawOpen(exact,O_RDONLY|O_CLOEXEC|O_NONBLOCK,0);
  if(fd<0) goto done;
  if(osFstat(fd,&actual)!=0 || !S_ISREG(actual.st_mode)
      || actual.st_dev!=expected.st_dev || actual.st_ino!=expected.st_ino
      || actual.st_size!=expected.st_size) goto done;
  while(total<wanted){
    ssize_t count=pread(fd,buffer+total,(size_t)(wanted-total),(off_t)(offset+(sqlite3_uint64)total));
    /* No EINTR retry and at most capacity advancing reads under the gate. */
    if(count<=0) goto done;
    total+=(int)count;
  }
  if(osFstat(fd,&actual)!=0 || actual.st_size!=expected.st_size
      || actual.st_mtim.tv_sec!=expected.st_mtim.tv_sec || actual.st_mtim.tv_nsec!=expected.st_mtim.tv_nsec
      || actual.st_ctim.tv_sec!=expected.st_ctim.tv_sec || actual.st_ctim.tv_nsec!=expected.st_ctim.tv_nsec) goto done;
  *written=total; rc=SQLITE_OK;
done:
  if(fd>=0 && close(fd)!=0){ rc=SQLITE_IOERR_CLOSE; *written=0; }
  pthread_mutex_unlock(&ryeosGateMutex);
  return rc;
}

static void ryeosForgetRetirement(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  RyeosBinding **link;
  pthread_mutex_lock(&ryeosGateMutex);
  for(link=&ryeosRetiring;*link && *link!=c;link=&(*link)->retireNext){}
  if(*link==c) *link=c->retireNext;
  ryeosGateFlushLocked();
  pthread_mutex_unlock(&ryeosGateMutex);
}
static RyeosBinding *ryeosPending = 0;
static RyeosBinding *ryeosReadyCopies = 0;
/* Resource release may happen in an unrelated SQLite callback. Retain the
   original node until Rust can destroy its opaque lease outside Unix global. */
static void ryeosRetireContextLocked(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  int i;
  assert(!c->vfsLinked); /* reaper retains every still-linked context */
  ryeosForgetRetirement(c);
  for(i=0;i<3;i++){ sqlite3_free(c->names[i]); sqlite3_free(c->paths[i]); }
  sqlite3_free(c->vfsName);
  if(c->copyLease){ c->pendingNext=ryeosReadyCopies; ryeosReadyCopies=c; }
  else { ryeosFreeStoppedNodes(c); ryeosNativeBindingDestroyed(c); sqlite3_free(c); }
}
SQLITE_API void *ryeos_sqlite_take_ready_copy_lease(void){
  if(!ryeos_native_process_matches()){return 0;}
  RyeosBinding *c; void *lease=0;
  unixEnterMutex();
  c=ryeosReadyCopies;
  if(c){
    assert(!c->vfsLinked); /* entered the ready list only after unlink */
    ryeosReadyCopies=c->pendingNext; lease=c->copyLease;
    ryeosFreeStoppedNodes(c); ryeosNativeBindingDestroyed(c); sqlite3_free(c); /* context is gone before any Rust lease destructor */
  }
  unixLeaveMutex();
  return lease;
}
static int ryeosIsBound(sqlite3_vfs *v){
  if(!ryeos_native_process_matches()){return 0;} return v && v->xOpen==ryeosBoundOpen; }
static RyeosBinding *ryeosContext(sqlite3_vfs *v){
  if(!ryeos_native_process_matches()){return 0;} return ryeosIsBound(v) ? (RyeosBinding*)v : 0; }
static int ryeosSameFd(int a,int b){
  if(!ryeos_native_process_matches()){return 0;}
  struct stat x,y;
  return a>=0 && b>=0 && osFstat(a,&x)==0 && osFstat(b,&y)==0
      && S_ISREG(x.st_mode) && S_ISREG(y.st_mode) && x.st_dev==y.st_dev && x.st_ino==y.st_ino;
}
static int ryeosStoppedCheck(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  int n,i; struct stat current,pin;
  for(n=0;n<c->stoppedCount;n++){
    int role=-1; for(i=0;i<3;i++) if(c->stoppedSelected[i]==n) role=i;
    if(role<0){
      if(fstatat(c->root,c->stoppedNames[n],&current,AT_SYMLINK_NOFOLLOW)==0 || errno!=ENOENT) return SQLITE_IOERR;
    }else{
      if(fstatat(c->root,c->stoppedNames[n],&current,AT_SYMLINK_NOFOLLOW)!=0 || !S_ISREG(current.st_mode)
          || (sqlite3_uint64)current.st_dev!=c->stoppedDev[role] || (sqlite3_uint64)current.st_ino!=c->stoppedIno[role]
          || (sqlite3_uint64)current.st_size!=c->stoppedLength[role]) return SQLITE_IOERR;
      if(c->pins[role]>=0 && (osFstat(c->pins[role],&pin)!=0 || !S_ISREG(pin.st_mode)
          || pin.st_dev!=current.st_dev || pin.st_ino!=current.st_ino)) return SQLITE_IOERR;
    }
  }
  return SQLITE_OK;
}
static int ryeosCheck(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  int i; struct stat s;
  if(c->poisoned) return SQLITE_IOERR;
  if(c->stoppedIntake) return ryeosStoppedCheck(c);
  if(osFstat(c->root,&s)!=0 || !S_ISDIR(s.st_mode)) goto bad;
  for(i=0;i<3;i++){
    if(c->files[i]<0){
      if(fstatat(c->root,c->names[i],&s,AT_SYMLINK_NOFOLLOW)==0 || errno!=ENOENT) goto bad;
      continue;
    }
    if(fstatat(c->root,c->names[i],&s,AT_SYMLINK_NOFOLLOW)!=0) goto bad;
    { struct stat expected;
      if(osFstat(c->files[i],&expected)!=0 || !S_ISREG(s.st_mode)
         || s.st_dev!=expected.st_dev || s.st_ino!=expected.st_ino) goto bad;
    }
  }
  /* Roles are distinct physical files. Header interpretation cannot make
     a main inode safe to adopt as WAL/SHM (DMS truncation is pre-schema). */
  { int a,b; struct stat left,right;
    for(a=0;a<3;a++) if(c->files[a]>=0){
      if(osFstat(c->files[a],&left)!=0) goto bad;
      for(b=a+1;b<3;b++) if(c->files[b]>=0){
        if(osFstat(c->files[b],&right)!=0
           || (left.st_dev==right.st_dev && left.st_ino==right.st_ino)) goto bad;
      }
    }
  }
  { char journal[256];
    if(snprintf(journal,sizeof(journal),"%s-journal",c->names[0]) >= (int)sizeof(journal)) goto bad;
    if(fstatat(c->root,journal,&s,AT_SYMLINK_NOFOLLOW)==0 || errno!=ENOENT) goto bad;
  }
  return SQLITE_OK;
bad: c->poisoned=1; return SQLITE_IOERR;
}
static int ryeosIndex(RyeosBinding *c,const char *path){
  if(!ryeos_native_process_matches()){return -1;}
  int i; if(!path) return -1;
  for(i=0;i<3;i++) if(strcmp(path,c->paths[i])==0) return i;
  return -1;
}
static int ryeosBoundFd(sqlite3_vfs *v,const char *path,int fd,int flags){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  RyeosBinding *c=ryeosContext(v); int i;
  if(!c) return SQLITE_OK;
  i=ryeosIndex(c,path);
  if(i<0 || ryeosCheck(c)!=SQLITE_OK || !ryeosSameFd(fd,c->files[i])){
    /* Only unadmitted unixOpen FDs transfer ownership here. Existing live
       file validation must never duplicate SQLite's descriptor ownership. */
    if(flags && fd>=0) c->failureFd=fd;
    c->poisoned=1; return SQLITE_CANTOPEN;
  }
  if(flags) ryeosGateOwner(fd,c);
  return SQLITE_OK;
}
static int ryeosBoundOpenFd(sqlite3_vfs *v,const char *path,int flags,int mode){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  RyeosBinding *c=ryeosContext(v); int i,fd;
  if(!c) return robust_open(path,flags,mode);
  i=ryeosIndex(c,path);
  if(i<0 || ryeosCheck(c)!=SQLITE_OK){ errno=EACCES; return -1; }
  if(c->readonlySource){
    /* A real readonly fallback must see the original denial without poisoning
       custody, so SQLite reports actual READONLY flags for WAL and SHM. No
       absent source role is ever created/repaired, regardless of open flags. */
    if(c->files[i]<0){errno=ENOENT;return -1;}
    if((flags&O_ACCMODE)!=O_RDONLY || (flags&(O_TRUNC|O_APPEND))){errno=EACCES;return -1;}
  }
  /* Existing sources never create/truncate; missing sidecars are acquired
     exclusively below, before SQLite may mutate their admitted descriptor. */
  flags &= ~(O_CREAT|O_EXCL|O_TRUNC);
  if(c->files[i]<0){
    /* First sidecar adoption: create ONLY the absent pinned child. A race
       producing EEXIST refuses, never adopts another owner's file. */
    if(i==0){ errno=EACCES; return -1; }
    fd=ryeosGateCreateAt(c,c->names[i]);
    if(fd<0){ c->poisoned=1; return -1; }
    c->files[i]=fcntl(fd,F_DUPFD_CLOEXEC,0);
    c->pins[i]=fcntl(fd,F_DUPFD_CLOEXEC,0);
    if(c->files[i]<0 || c->pins[i]<0 || fsync(c->root)!=0){ c->failureFd=fd; c->poisoned=1; return -1; }
  }else{
    char pinned[64]; struct stat regular;
    if(c->pins[i]<0 || osFstat(c->pins[i],&regular)!=0 || !S_ISREG(regular.st_mode)){
      c->poisoned=1; errno=EACCES; return -1;
    }
    snprintf(pinned,sizeof(pinned),"/proc/self/fd/%d",c->pins[i]);
    /* Intentional proc descriptor symlink to the already proven regular inode,
       never the replaceable child entry. Do not pass O_NOFOLLOW here. */
    fd=robust_open(pinned,(flags & ~O_NOFOLLOW)|O_CLOEXEC,mode);
  }
  if(fd<0 || !ryeosSameFd(fd,c->files[i]) || ryeosCheck(c)!=SQLITE_OK){
    if(fd>=0) c->failureFd=fd; /* fixed reachable failure keeper */
    c->poisoned=1; errno=EACCES; return -1;
  }
  if(fd>=0) ryeosGateOwner(fd,c);
  return fd;
}
static int ryeosBoundFile(unixFile *f){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  if(!f || !ryeosIsBound(f->pVfs)) return SQLITE_OK;
  return ryeosBoundFd(f->pVfs,f->zPath,f->h,0);
}
static int ryeosBoundWritable(unixFile *f){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=f?ryeosContext(f->pVfs):0;
  if(c && c->readonlySource)return SQLITE_READONLY;
  return ryeosBoundFile(f);
}
static int ryeosBoundShm(unixFile *f,unixShmNode *n){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=ryeosContext(f->pVfs);
  if(!c) return SQLITE_OK;
  if(ryeosBoundFile(f)!=SQLITE_OK || !n || !ryeosSameFd(n->hShm,c->files[2])
      || (c->readonlySource && !n->isReadonly)){
    c->poisoned=1; return SQLITE_IOERR_SHMOPEN;
  }
  return SQLITE_OK;
}
static int ryeosRejectShmAdoption(RyeosBinding *c,unixShmNode *n,int rc){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  /* A new node has not acquired DMS locks or mappings. Stock error purge
     must not close its FD and unlock an existing physical alias node. */
  if(c && n && n->nRef==0 && n->hShm>=0){
    c->failureFd=n->hShm; n->hShm=-1;
  }
  return rc;
}
static int ryeosBoundShmAdopt(unixFile *f,unixShmNode *n){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=ryeosContext(f->pVfs);
  int rc=ryeosBoundShm(f,n);
  if(!c) return rc;
  if(rc!=SQLITE_OK) return ryeosRejectShmAdoption(c,n,rc);
  /* Called with the Unix global mutex during adoption. Distinct main
     inode nodes must not share one physical SHM: stock purge would close
     their descriptor and release each other's process POSIX locks. The
     legitimate same-main reuse has the identical node pointer. */
  assert(unixMutexHeld());
  { unixInodeInfo *inode; struct stat expected, actual;
    if(osFstat(n->hShm,&expected)!=0){ c->poisoned=1; return ryeosRejectShmAdoption(c,n,SQLITE_IOERR_SHMOPEN); }
    for(inode=inodeList;inode;inode=inode->pNext){
      unixShmNode *other=inode->pShmNode;
      if(other && other!=n && other->nRef>0 && other->hShm>=0){
        if(osFstat(other->hShm,&actual)!=0
           || (actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino)){
          c->poisoned=1; return ryeosRejectShmAdoption(c,n,SQLITE_IOERR_SHMOPEN);
        }
      }
    }
  }
  return SQLITE_OK;
}
static int ryeosBoundShmOpen(unixFile *f,const char *path,int flags,int mode){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  return ryeosBoundOpenFd(f->pVfs,path,flags,mode);
}
static int ryeosBoundOpen(sqlite3_vfs *v,const char *path,sqlite3_file *f,int flags,int *out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=ryeosContext(v); int i=ryeosIndex(c,path);
  if(ryeosCheck(c)!=SQLITE_OK || i<0 || i==2
      || (flags & (SQLITE_OPEN_DELETEONCLOSE|SQLITE_OPEN_EXCLUSIVE))
      || ((flags & SQLITE_OPEN_MAIN_DB) && i!=0)
      || ((flags & SQLITE_OPEN_WAL) && i!=1)
      || !(flags & (SQLITE_OPEN_MAIN_DB|SQLITE_OPEN_WAL))) return SQLITE_CANTOPEN;
  if(i==0){ int expected=0;
    if(!atomic_compare_exchange_strong(&c->mainClaimed,&expected,1)) return SQLITE_CANTOPEN;
  }
  { int rc=unixOpen(v,path,f,flags,out);
    if(rc==SQLITE_OK){ ((unixFile*)f)->ryeosCounted=1; atomic_fetch_add(&c->activeFiles,1); }
    return rc; }
}
static int ryeosBoundFullPath(sqlite3_vfs *v,const char *path,int n,char *out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=ryeosContext(v); int i=ryeosIndex(c,path);
  if(i!=0 || ryeosCheck(c)!=SQLITE_OK || (int)strlen(path)>=n) return SQLITE_CANTOPEN;
  memcpy(out,path,strlen(path)+1); return SQLITE_OK;
}
static int ryeosBoundDelete(sqlite3_vfs *v,const char *path,int sync){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  (void)v; (void)path; (void)sync;
  /* No check-then-unlink of a replaceable sibling. Persist WAL/SHM. */
  return SQLITE_IOERR_DELETE;
}
static int ryeosBoundAccess(sqlite3_vfs *v,const char *path,int flags,int *out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=ryeosContext(v); int i=ryeosIndex(c,path);
  char journal[512];
  *out=0;
  if(ryeosCheck(c)!=SQLITE_OK) return SQLITE_IOERR_ACCESS;
  if(i>=0){ struct stat s;
    /* check established exact absence; no fstat(-1), no implicit adoption. */
    if(c->files[i]<0) return SQLITE_OK;
    if(osFstat(c->files[i],&s)!=0) return SQLITE_IOERR_ACCESS;
    if(flags==SQLITE_ACCESS_EXISTS){ *out=(s.st_size>0); return SQLITE_OK; }
    if(flags==SQLITE_ACCESS_READWRITE){
      if(c->readonlySource)return SQLITE_OK; /* *out stays false */
      *out=(faccessat(c->root,c->names[i],R_OK|W_OK,AT_EACCESS)==0);
      return SQLITE_OK;
    }
    return SQLITE_IOERR_ACCESS;
  }
  if(snprintf(journal,sizeof(journal),"%s-journal",c->paths[0]) >= (int)sizeof(journal)) return SQLITE_IOERR_ACCESS;
  if(strcmp(path,journal)==0) return SQLITE_OK; /* absence proved by check */
  return SQLITE_IOERR_ACCESS;
}
static void *ryeosBoundClosing(unixFile *f){
  if(!ryeos_native_process_matches()){return 0;}
  return f->ryeosCounted ? ryeosContext(f->pVfs) : 0;
}
static void ryeosBoundClosed(void *p){
  if(!ryeos_native_process_matches()){return;}
  if(p) atomic_fetch_sub(&((RyeosBinding*)p)->activeFiles,1);
}
/* A retained FD may release POSIX locks on ANY same-inode SQLite handle.
   Defer until every main inode owner AND every physical SHM-node alias is
   gone. The Unix global mutex serializes adoption/reference transitions.
   No allocation is required on close/failure: context owns all keeper slots. */
static int ryeosFdLive(int fd){
  if(!ryeos_native_process_matches()){return 1;}
  struct stat expected, actual; unixInodeInfo *inode;
  if(osFstat(fd,&expected)!=0) return 2; /* unknown remains owned */
  for(inode=inodeList;inode;inode=inode->pNext){
    unixShmNode *node=inode->pShmNode;
    if(inode->nRef>0 && inode->fileId.dev==expected.st_dev
       && inode->fileId.ino==(u64)expected.st_ino) return 1;
    if(node && node->nRef>0 && node->hShm>=0){
      if(osFstat(node->hShm,&actual)!=0) return 2;
      if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino) return 1;
    }
  }
  return 0;
}
/* Original Rust output retirement. No allocation/reaper/callback, one-shot
   Unix-global (or install claim when SQLite is inactive) -> gate trylocks
   and precharged finite identity traversal. */
static int ryeosOutputScanStep(int *remaining){
  if(!ryeos_native_process_matches()){return 0;}
  if(*remaining<=0) return 0;
  --*remaining; return 1;
}
/* O_PATH never participates in POSIX data-FD locks. No SQLite state,
   allocation, callback, alias scan or linked-file unlink is needed here. */
/* Actual transferred finalizer descriptor close. Trusted original host
   verifies ACK, alias/lock registry custody and per-owner retirement order;
   this fixed syscall path proves no unlink or aggregate lock release. */
SQLITE_API int ryeos_sqlite_close_original_finalizer_file(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int kind,int *consumed,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat observed; int flags;
  if(consumed)*consumed=0;
  if(observed_errno)*observed_errno=0;
  if(!consumed || !observed_errno || fd<0 || kind<0 || kind>3)return SQLITE_MISUSE;
  flags=fcntl(fd,F_GETFL);
  if(flags<0){*observed_errno=errno;return SQLITE_IOERR;}
  if(flags&O_PATH)return SQLITE_MISMATCH;
  if(fstat(fd,&observed)!=0){*observed_errno=errno;return SQLITE_IOERR_FSTAT;}
  if((kind==0 || kind==3) ? !S_ISREG(observed.st_mode) : !S_ISDIR(observed.st_mode))return SQLITE_MISMATCH;
  if((sqlite3_uint64)observed.st_dev!=dev || (sqlite3_uint64)observed.st_ino!=ino)return SQLITE_MISMATCH;
  *consumed=1;
  if(close(fd)==0)return SQLITE_OK;
  *observed_errno=errno;return SQLITE_IOERR_CLOSE;
}
SQLITE_API int ryeos_sqlite_close_original_metadata_pin(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int *consumed,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat observed; int flags;
  if(consumed)*consumed=0;
  if(observed_errno)*observed_errno=0;
  if(!consumed || !observed_errno || fd<0)return SQLITE_MISUSE;
  flags=fcntl(fd,F_GETFL);
  if(flags<0){*observed_errno=errno;return SQLITE_IOERR;}
  if((flags&O_PATH)!=O_PATH)return SQLITE_MISMATCH;
  if(fstat(fd,&observed)!=0){*observed_errno=errno;return SQLITE_IOERR_FSTAT;}
  if(!S_ISREG(observed.st_mode) || (sqlite3_uint64)observed.st_dev!=dev
     || (sqlite3_uint64)observed.st_ino!=ino)return SQLITE_MISMATCH;
  *consumed=1;
  if(close(fd)==0)return SQLITE_OK;
  *observed_errno=errno;return SQLITE_IOERR_CLOSE;
}
/* Identity and alias census shared by both retirement paths. The caller
   holds the gate mutex and has excluded every inodeList transition, either
   with the Unix global mutex or by proving SQLite inactive. Returns
   SQLITE_OK only when the descriptor is the unlinked original regular file
   and no SQLite inode, SHM node, gate file or pending/retiring binding
   context refers to that inode; an alias refuses with SQLITE_BUSY. Every
   visit is charged to the same finite scan bound. */
static int ryeosOutputAliasScan(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int *remaining,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat expected,actual; unixInodeInfo *inode; RyeosGateFd *node;
  RyeosBinding *context; int list,i;
  if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
  if(osFstat(fd,&expected)!=0){ *observed_errno=errno; return SQLITE_IOERR_FSTAT; }
  if(!S_ISREG(expected.st_mode) || (sqlite3_uint64)expected.st_dev!=dev
      || (sqlite3_uint64)expected.st_ino!=ino || expected.st_nlink!=0) return SQLITE_MISMATCH;
  for(inode=inodeList;inode;inode=inode->pNext){
    unixShmNode *shm;
    if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
    if(inode->nRef>0 && inode->fileId.dev==expected.st_dev && inode->fileId.ino==(u64)expected.st_ino) return SQLITE_BUSY;
    shm=inode->pShmNode;
    if(shm && shm->nRef>0 && shm->hShm>=0){
      if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
      if(osFstat(shm->hShm,&actual)!=0){ *observed_errno=errno; return SQLITE_IOERR_FSTAT; }
      if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino) return SQLITE_BUSY;
    }
  }
  for(node=ryeosGateFiles;node;node=node->next){
    if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
    if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
    if(osFstat(node->fd,&actual)!=0){ *observed_errno=errno; return SQLITE_IOERR_FSTAT; }
    if(actual.st_dev!=node->dev || actual.st_ino!=node->ino) return SQLITE_MISMATCH;
    if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino) return SQLITE_BUSY;
  }
  /* Pending contexts can also be present in retiring; duplicate visits are
     charged, never omitted on that assumption. */
  for(list=0;list<2;list++) for(context=list?ryeosPending:ryeosRetiring;context;context=list?context->pendingNext:context->retireNext){
    if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
    if(context->stoppedIntake) for(i=0;i<3;i++){
      if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
      if(context->stoppedSelected[i]>=0 && context->stoppedDev[i]==dev && context->stoppedIno[i]==ino) return SQLITE_BUSY;
    }
    for(i=0;i<7;i++){
      int other=i<3?context->files[i]:i<6?context->pins[i-3]:context->failureFd;
      if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
      if(other<0) continue;
      if(!ryeosOutputScanStep(remaining)) return SQLITE_TOOBIG;
      if(osFstat(other,&actual)!=0){ *observed_errno=errno; return SQLITE_IOERR_FSTAT; }
      if(actual.st_dev==expected.st_dev && actual.st_ino==expected.st_ino) return SQLITE_BUSY;
    }
  }
  return SQLITE_OK;
}
static int ryeosOutputCloseCleared(int fd,int *consumed,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  /* Linux consumes the coordinate even on late error/EINTR. NEVER retry it. */
  *consumed=1;
  if(close(fd)==0) return SQLITE_OK;
  *observed_errno=errno; return SQLITE_IOERR_CLOSE;
}
/* Retirement while SQLite is inactive: never initialized, or shut down so
   that sqlite3_os_end cleared the Unix global mutex. With that mutex absent
   nothing can be taken to freeze inodeList, so inactivity itself is proved
   and held for the whole census and close:
   - The install claim is taken first. sqlite3_initialize passes its native
     gate only inside a scope of the installed arena, and an arena is
     installed only by the caller that wins this same claim. While it is
     held no arena can appear, so no thread can initialize SQLite, register
     a VFS, open a file, create a binding or allocate a gate node.
   - A claim won with no installed arena also means no thread is inside an
     arena scope: release unmaps an arena only with zero scopes and with
     SQLite uninitialized, so isInit, the Unix global mutex and inodeList
     are stable and must read as inactive and empty.
   - The gate mutex is then taken as on the locked path, and the shared
     census still runs over every gate file and binding context.
   A held claim, an installed arena or any sign of live SQLite refuses
   unconsumed with SQLITE_BUSY; the caller keeps its descriptor. */
static int ryeosCloseOriginalOutputInactive(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int maximum,int *consumed,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  int rc,remaining=maximum,locked,expected=0;
  if(!atomic_compare_exchange_strong(&ryeosNativeInstall,&expected,1)) return SQLITE_BUSY;
  if(atomic_load(&ryeosNativeOwner) || sqlite3GlobalConfig.isInit || unixBigLock || inodeList){
    atomic_store(&ryeosNativeInstall,0); return SQLITE_BUSY;
  }
  locked=pthread_mutex_trylock(&ryeosGateMutex);
  if(locked){ *observed_errno=locked; atomic_store(&ryeosNativeInstall,0); return SQLITE_BUSY; }
  rc=ryeosOutputAliasScan(fd,dev,ino,&remaining,observed_errno);
  if(rc==SQLITE_OK) rc=ryeosOutputCloseCleared(fd,consumed,observed_errno);
  pthread_mutex_unlock(&ryeosGateMutex);
  atomic_store(&ryeosNativeInstall,0);
  return rc;
}
SQLITE_API int ryeos_sqlite_close_original_output(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int maximum,int *consumed,int *observed_errno){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  int rc,remaining=maximum,locked;
  if(consumed) *consumed=0;
  if(observed_errno) *observed_errno=0;
  if(!consumed || !observed_errno || fd<0 || maximum<=0) return SQLITE_MISUSE;
  if(!unixBigLock) return ryeosCloseOriginalOutputInactive(fd,dev,ino,maximum,consumed,observed_errno);
  if(sqlite3_mutex_try(unixBigLock)!=SQLITE_OK) return SQLITE_BUSY;
  locked=pthread_mutex_trylock(&ryeosGateMutex);
  if(locked){ *observed_errno=locked; sqlite3_mutex_leave(unixBigLock); return SQLITE_BUSY; }
  rc=ryeosOutputAliasScan(fd,dev,ino,&remaining,observed_errno);
  if(rc==SQLITE_OK) rc=ryeosOutputCloseCleared(fd,consumed,observed_errno);
  pthread_mutex_unlock(&ryeosGateMutex);
  sqlite3_mutex_leave(unixBigLock);
  return rc;
}
static void ryeosReapKeepersLocked(void){
  if(!ryeos_native_process_matches()){return;}
  RyeosBinding **link=&ryeosPending;
  assert(unixMutexHeld());
  while(*link){
    RyeosBinding *c=*link; int i,remaining=0;
    c->retentionReason=c->closeErrno ? 4 : 0;
    if((c->retirePhase>0 && c->retirePhase<3) || c->abandonedDb || atomic_load(&c->activeFiles)!=0){
      if(c->retentionReason<3) c->retentionReason=3;
      link=&c->pendingNext; continue;
    }
    for(i=0;i<7;i++){
      _Atomic int *slot=i<3 ? &c->files[i] : i<6 ? &c->pins[i-3] : &c->failureFd;
      if(*slot>=0){
        int reason=ryeosFdLive(*slot);
        if(reason){
          remaining=1;
          if(c->retentionReason<reason) c->retentionReason=reason;
          continue;
        }
        /* Linux releases the descriptor even if close reports late I/O error
           or EINTR. Never retry the numeric FD and close a reused coordinate.
           Preserve the observed error in original native custody instead. */
        { int closed=ryeosGateClose(*slot,c,slot);
          if(closed==1){ remaining=1; c->retentionReason=1; continue; }
          if(closed<0){ c->closeErrno=errno; c->retentionReason=4; }
          *slot=-1;
        }
      }
    }
    if(c->trackedNodes){ remaining=1; if(!c->retentionReason) c->retentionReason=3; }
    if(c->vfsLinked){ if(!c->retentionReason) c->retentionReason=3; remaining=1; }
    if(remaining || c->callerHeld || c->closeErrno){ link=&c->pendingNext; continue; }
    if(c->root>=0){
      int rc=close(c->root); c->root=-1;
      if(rc!=0){ c->closeErrno=errno; c->retentionReason=4; link=&c->pendingNext; continue; }
    }
    *link=c->pendingNext;
    ryeosRetireContextLocked(c);
  }
}
/* Invariant: unlinking a bound VFS does not depend on scope or init state.
   sqlite3_vfs_unregister runs sqlite3_initialize, which the native init gate
   refuses outside a scope; a skipped unlink would leave freed context memory
   on vfsList. The static main mutex comes from methods copied once at the
   first initialization and never cleared, and a linked binding exists only
   after that, so this path takes it directly. Idempotent; a context is never
   freed nor uncounted while vfsLinked is set. */
static void ryeosBindingUnlink(RyeosBinding *c){
  if(!ryeos_native_process_matches()){return;}
  sqlite3_mutex *mutex;
  if(!c->vfsLinked) return;
  mutex=sqlite3MutexAlloc(SQLITE_MUTEX_STATIC_MAIN);
  sqlite3_mutex_enter(mutex);
  vfsUnlink(&c->vfs);
  sqlite3_mutex_leave(mutex);
  c->vfsLinked=0;
}
/* Only the unique caller initiates release. VFS unlink precedes taking
   Unix global, avoiding an inverted registry/global mutex order. List and
   caller-held transitions, progress and destruction are global-locked. */
static void ryeosQueue(RyeosBinding *c,int callerHeld){
  if(!ryeos_native_process_matches()){return;}
  c->callerHeld=callerHeld;
  if(!c->queued){
    c->queued=1; c->pendingNext=ryeosPending; ryeosPending=c;
  }
}
SQLITE_API int ryeos_sqlite_binding_release(void *handle,void **lease){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)handle; RyeosBinding **link; int i,reason;
  *lease=0;
  if(!c) return 2;
  ryeosBindingUnlink(c);
  unixEnterMutex();
  ryeosQueue(c,1);
  ryeosReapKeepersLocked(); /* callerHeld forbids context auto-free */
  reason=c->retentionReason;
  if(!reason){
    for(i=0;i<3;i++) if(c->files[i]>=0 || c->pins[i]>=0) reason=2;
    if(c->failureFd>=0 || atomic_load(&c->activeFiles)!=0 || c->trackedNodes || c->vfsLinked) reason=3;
  }
  if(!reason && c->root>=0){
    int rc=close(c->root); c->root=-1;
    if(rc!=0){ c->closeErrno=errno; reason=c->retentionReason=4; }
  }
  if(!reason){
    for(link=&ryeosPending;*link && *link!=c;link=&(*link)->pendingNext){}
    assert(*link==c); *link=c->pendingNext;
    ryeosForgetRetirement(c);
    for(i=0;i<3;i++){ sqlite3_free(c->names[i]); sqlite3_free(c->paths[i]); }
    sqlite3_free(c->vfsName);
    *lease=c->copyLease;
    ryeosFreeStoppedNodes(c); ryeosNativeBindingDestroyed(c); sqlite3_free(c);
  }
  unixLeaveMutex();
  return reason; /* zero means actual complete physical release */
}
SQLITE_API void ryeos_sqlite_binding_drop(void *handle){
  if(!ryeos_native_process_matches()){return;}
  RyeosBinding *c=(RyeosBinding*)handle;
  if(!c) return;
  ryeosBindingUnlink(c);
  unixEnterMutex();
  ryeosQueue(c,0); /* fallback native owner, not supervisor testimony */
  ryeosReapKeepersLocked();
  unixLeaveMutex();
}
SQLITE_API void ryeos_sqlite_binding_abandon(void *handle,sqlite3 *db){
  if(!ryeos_native_process_matches()){return;}
  RyeosBinding *c=(RyeosBinding*)handle;
  if(!c) return;
  ryeosBindingUnlink(c);
  unixEnterMutex();
  c->abandonedDb=db; /* private Rust owner passes its exact failed-close DB */
  ryeosQueue(c,0);
  ryeosReapKeepersLocked(); /* retains actual DB/context; no cleanup success */
  unixLeaveMutex();
}
/* Captured role claims precede SQL close. Native keeper closure and stock
   descriptor acquisition both consult the same claim under gate mutex. */
SQLITE_API int ryeos_sqlite_retirement_capture(void *handle,const char *generation){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)handle; RyeosGateFd *node; RyeosBinding *peer;
  int i,rc=SQLITE_OK;
  if(!c || !generation || strlen(generation)!=32 || c->readonlyMain
      || strcmp(c->names[0],"scheduler.sqlite3")!=0 || c->retirePhase) return SQLITE_MISUSE;
  for(i=0;i<32;i++) if(!((generation[i]>='0' && generation[i]<='9') ||
      (generation[i]>='a' && generation[i]<='f'))) return SQLITE_MISUSE;
  unixEnterMutex();
  pthread_mutex_lock(&ryeosGateMutex);
  if(ryeosCheck(c)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
  for(peer=ryeosRetiring;peer;peer=peer->retireNext){
    for(i=0;i<3;i++) if(c->files[i]>=0 && ryeosGateMatches(peer,c->files[i])){
      rc=SQLITE_BUSY; goto done;
    }
  }
  for(node=ryeosGateFiles;node;node=node->next){
    if(node->owner!=c){
      int match=ryeosGateMatches(c,node->fd);
      if(match){ rc=match==1?SQLITE_BUSY:SQLITE_IOERR; goto done; }
    }
  }
  for(i=0;i<3;i++){
    const char *role=i==0?"main":i==1?"wal":"shm";
    snprintf(c->retirementNames[i][0],sizeof(c->retirementNames[i][0]),
        "scheduler-retirement.role.%s.%s.quarantine",generation,role);
    snprintf(c->retirementNames[i][1],sizeof(c->retirementNames[i][1]),
        "scheduler-retirement.role.%s.%s.purge",generation,role);
  }
  c->retirePhase=1; c->retireNext=ryeosRetiring; ryeosRetiring=c;
done:
  pthread_mutex_unlock(&ryeosGateMutex);
  unixLeaveMutex();
  return rc;
}
SQLITE_API int ryeos_sqlite_retirement_barriers(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)handle; struct flock lock; int i,rc=SQLITE_OK;
  if(!c || (c->retirePhase!=1 && c->retirePhase!=2) || (c->stoppedIntake && !c->stoppedAcquired)) return SQLITE_MISUSE;
  unixEnterMutex();
  pthread_mutex_lock(&ryeosGateMutex);
  if(c->abandonedDb || atomic_load(&c->activeFiles)!=0 || c->closeErrno){ rc=SQLITE_IOERR; goto done; }
  if(ryeosCheck(c)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
  memset(&lock,0,sizeof(lock)); lock.l_whence=SEEK_SET; lock.l_type=F_WRLCK;
  /* Same-TU regions on every retained role also exclude supported external
     Unix SQLite peers that name a role inode as a different main/SHM role.
     Partial acquisition remains held; a busy result permits no role effect. */
  for(i=0;i<3;i++) if(c->files[i]>=0){
    lock.l_start=PENDING_BYTE; lock.l_len=SHARED_SIZE+2;
    if(osFcntl(c->files[i],F_SETLK,&lock)!=0){
      rc=(errno==EACCES || errno==EAGAIN)?SQLITE_BUSY:SQLITE_IOERR_LOCK; goto done;
    }
    c->barrierMain=1;
    lock.l_start=UNIX_SHM_BASE; lock.l_len=SQLITE_SHM_NLOCK+1;
    if(osFcntl(c->files[i],F_SETLK,&lock)!=0){
      rc=(errno==EACCES || errno==EAGAIN)?SQLITE_BUSY:SQLITE_IOERR_SHMLOCK; goto done;
    }
    c->barrierShm=1;
  }
  c->retirePhase=2;
done:
  pthread_mutex_unlock(&ryeosGateMutex);
  unixLeaveMutex();
  return rc;
}
SQLITE_API int ryeos_sqlite_retirement_role_fd(void *handle,int role){
  if(!ryeos_native_process_matches()){errno=EPERM;return -1;}
  RyeosBinding *c=(RyeosBinding*)handle;
  return c && role>=0 && role<3 && c->retirePhase>0 ? (c->files[role]>=0 ? c->files[role] : c->stoppedIntake ? c->pins[role] : -1) : -1;
}
SQLITE_API int ryeos_sqlite_retirement_allow_release(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)handle;
  if(!c || (c->retirePhase!=1 && c->retirePhase!=2)) return SQLITE_MISUSE;
  pthread_mutex_lock(&ryeosGateMutex);
  c->retirePhase=3;
  pthread_mutex_unlock(&ryeosGateMutex);
  return SQLITE_OK;
}

/* Private no-SQL stopped intake, names derived/validated by Rust host.
   Claims install before data opens. Every partial pin/node is caller-owned. */
SQLITE_API int ryeos_sqlite_stopped_create(int root,const char *const *names,int count,
    const int *selected,const sqlite3_uint64 *dev,const sqlite3_uint64 *ino,
    const sqlite3_uint64 *length,void **out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c; int i,j,rc=SQLITE_CANTOPEN; struct stat actual;
  *out=0;
  if(!ryeosNativeContext(ryeosNativeScope) || !ryeosNativeCleanup
      || ryeosNativeOpenTicket)return SQLITE_MISUSE;
  if(count<1 || count>100 || !names || !selected || !dev || !ino || !length) return SQLITE_MISUSE;
  for(i=0;i<count;i++) if(!names[i] || !*names[i] || strlen(names[i])>=160 || strchr(names[i],'/')) return SQLITE_MISUSE;
  for(i=0;i<3;i++) if(selected[i]<-1 || selected[i]>=count) return SQLITE_MISUSE;
  c=sqlite3_malloc64(sizeof(*c)); if(!c) return SQLITE_NOMEM;
  memset(c,0,sizeof(*c)); c->root=-1; c->failureFd=-1;
  ryeosNativeBindingRegister(c);
  c->stoppedIntake=1; c->retirePhase=1; c->stoppedCount=count;
  for(i=0;i<3;i++){
    c->files[i]=-1; c->pins[i]=-1; c->stoppedSelected[i]=selected[i];
    c->stoppedDev[i]=dev[i]; c->stoppedIno[i]=ino[i]; c->stoppedLength[i]=length[i];
  }
  *out=c; /* every subsequent error returns exact context */
  for(i=0;i<count;i++) strcpy(c->stoppedNames[i],names[i]);
  c->root=fcntl(root,F_DUPFD_CLOEXEC,0);
  if(c->root<0 || osFstat(c->root,&actual)!=0 || !S_ISDIR(actual.st_mode)) return rc;
  for(i=0;i<3;i++){
    c->stoppedNodes[i]=sqlite3_malloc64(sizeof(RyeosGateFd));
    if(!c->stoppedNodes[i]) return SQLITE_NOMEM;
    memset(c->stoppedNodes[i],0,sizeof(RyeosGateFd)); c->stoppedNodes[i]->fd=-1; c->stoppedNodes[i]->pin=-1;
    c->names[i]=sqlite3_mprintf("%s",selected[i]>=0?names[selected[i]]:i==0?"scheduler.sqlite3":i==1?"scheduler.sqlite3-wal":"scheduler.sqlite3-shm");
    if(!c->names[i]) return SQLITE_NOMEM;
  }
  unixEnterMutex(); pthread_mutex_lock(&ryeosGateMutex);
  for(i=0;i<count;i++) if(ryeosGateNamespaceBlockedLocked(c->root,names[i])) goto done;
  for(i=0;i<3;i++) for(j=i+1;j<3;j++) if(selected[i]>=0 && selected[j]>=0 && dev[i]==dev[j] && ino[i]==ino[j]) goto done;
  c->retireNext=ryeosRetiring; ryeosRetiring=c;
  rc=SQLITE_OK;
done: pthread_mutex_unlock(&ryeosGateMutex); unixLeaveMutex(); return rc;
}
SQLITE_API int ryeos_sqlite_stopped_acquire(void *handle){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)handle; RyeosGateFd *node; int i,rc=SQLITE_OK; struct stat actual;
  if(!c || !c->stoppedIntake || (c->retirePhase!=1 && c->retirePhase!=2) || c->poisoned || c->root<0) return SQLITE_MISUSE;
  /* Finish preallocation on SAME partial context before any new data open. */
  for(i=0;i<3;i++){
    if(!c->stoppedNodes[i] && c->files[i]<0){
      c->stoppedNodes[i]=sqlite3_malloc64(sizeof(RyeosGateFd));
      if(!c->stoppedNodes[i]) return SQLITE_NOMEM;
      memset(c->stoppedNodes[i],0,sizeof(RyeosGateFd)); c->stoppedNodes[i]->fd=-1; c->stoppedNodes[i]->pin=-1;
    }
    if(!c->names[i]){
      c->names[i]=sqlite3_mprintf("%s",c->stoppedSelected[i]>=0?c->stoppedNames[c->stoppedSelected[i]]:i==0?"scheduler.sqlite3":i==1?"scheduler.sqlite3-wal":"scheduler.sqlite3-shm");
      if(!c->names[i]) return SQLITE_NOMEM;
    }
  }
  unixEnterMutex(); pthread_mutex_lock(&ryeosGateMutex);
  { RyeosBinding *claim; for(claim=ryeosRetiring;claim && claim!=c;claim=claim->retireNext){}
    if(!claim){
      for(i=0;i<c->stoppedCount;i++) if(ryeosGateNamespaceBlockedLocked(c->root,c->stoppedNames[i])){ rc=SQLITE_BUSY; goto done; }
      c->retireNext=ryeosRetiring; ryeosRetiring=c;
    }
  }
  if(ryeosStoppedCheck(c)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
  for(i=0;i<3;i++) if(c->stoppedSelected[i]>=0 && c->pins[i]<0){
    c->pins[i]=openat(c->root,c->names[i],O_PATH|O_NOFOLLOW|O_CLOEXEC);
    if(c->pins[i]<0 || osFstat(c->pins[i],&actual)!=0 || !S_ISREG(actual.st_mode)
        || (sqlite3_uint64)actual.st_dev!=c->stoppedDev[i] || (sqlite3_uint64)actual.st_ino!=c->stoppedIno[i]){
      rc=SQLITE_IOERR; goto done;
    }
  }
  if(!c->stoppedPhysical){
    ryeosGateFlushLocked(); /* pending O_PATH claims do NOT defer incumbent close */
    if(ryeosGateOpening){ rc=SQLITE_BUSY; goto done; }
    for(node=ryeosGateFiles;node;node=node->next) if(node->owner!=c && ryeosGateMatches(c,node->fd)){
      rc=SQLITE_BUSY; goto done; /* active/inflight/closing keeper inventory */
    }
    c->stoppedPhysical=1; /* atomic with audit, before ANY data open */
  }
  for(i=0;i<3;i++) if(c->stoppedSelected[i]>=0 && c->files[i]<0){
    char pinned[64]; int fd;
    node=c->stoppedNodes[i]; if(!node){ rc=SQLITE_NOMEM; goto done; }
    if(ryeosLeaseBlocksFd(c->pins[i])){rc=SQLITE_BUSY;goto done;}
    snprintf(pinned,sizeof(pinned),"/proc/self/fd/%d",c->pins[i]);
    fd=ryeosGateRawOpen(pinned,O_RDWR|O_CLOEXEC,0); /* exact private context exemption */
    if(fd<0){ rc=SQLITE_CANTOPEN; goto done; }
    c->files[i]=fd; node->fd=fd; node->owner=c; node->slot=&c->files[i]; c->trackedNodes++;
    node->next=ryeosGateFiles; ryeosGateFiles=node; c->stoppedNodes[i]=0;
    if(osFstat(fd,&actual)!=0){ c->closeErrno=EBADF; rc=SQLITE_IOERR; goto done; }
    node->dev=actual.st_dev; node->ino=actual.st_ino;
    if(!S_ISREG(actual.st_mode) || (sqlite3_uint64)actual.st_dev!=c->stoppedDev[i] || (sqlite3_uint64)actual.st_ino!=c->stoppedIno[i]){
      c->poisoned=1; rc=SQLITE_IOERR; goto done;
    }
  }
  if(ryeosStoppedCheck(c)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
  c->stoppedAcquired=1;
done: pthread_mutex_unlock(&ryeosGateMutex); unixLeaveMutex(); return rc;
}
SQLITE_API int ryeos_sqlite_binding_create(int root,const char *name,int mode,void *lease,void **out){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c; sqlite3_vfs *base; int i,rc=SQLITE_CANTOPEN; struct stat s;
  char journal[256];
  *out=0;
  /* Reject any new ordinary/SQL binding before allocator debit or FS effect.
     Original factory open alone arms this exact single-use native ticket. */
  if(!ryeosNativeContext(ryeosNativeScope) || ryeosNativeCleanup
      || !ryeosNativeOpenTicket)return SQLITE_MISUSE;
  if(!name || !*name || strlen(name)>200 || strchr(name,'/') || strcmp(name,".")==0 || strcmp(name,"..")==0) return rc;
  base=sqlite3_vfs_find("unix");
  if(!base || base->xOpen!=unixOpen || base->szOsFile!=sizeof(unixFile)) return SQLITE_NOTFOUND;
  c=sqlite3_malloc64(sizeof(*c)); if(!c) return SQLITE_NOMEM;
  memset(c,0,sizeof(*c)); c->copyLease=lease; c->root=-1; c->failureFd=-1; for(i=0;i<3;i++){ c->files[i]=-1; c->pins[i]=-1; }
  ryeosNativeBindingRegister(c);
  c->root=fcntl(root,F_DUPFD_CLOEXEC,0);
  if(c->root<0 || osFstat(c->root,&s)!=0 || !S_ISDIR(s.st_mode)) goto fail;
  if(ryeosGateNamespaceBlocked(c->root,name)) goto fail;
  if(mode<0 || mode>4) goto fail;
  if(mode==4){c->readonlySource=1;c->readonlyMain=1;mode=0;}
  if(mode==3){ c->readonlyMain=1; mode=0; }
  if(mode==2){
    if(fstatat(c->root,name,&s,AT_SYMLINK_NOFOLLOW)==0) mode=0;
    else if(errno==ENOENT) mode=1;
    else goto fail;
  }
  if(mode==1){
    /* Refuse all existing roles before any creation; exclusive main creation
       never adopts an occupant racing this check. */
    for(i=0;i<4;i++){
      char child[256];
      snprintf(child,sizeof(child),"%s%s",name,i==1?"-wal":i==2?"-shm":i==3?"-journal":"");
      if(fstatat(c->root,child,&s,AT_SYMLINK_NOFOLLOW)==0 || errno!=ENOENT) goto fail;
    }
  }
  snprintf(journal,sizeof(journal),"%s-journal",name);
  if(fstatat(c->root,journal,&s,AT_SYMLINK_NOFOLLOW)==0 || errno!=ENOENT) goto fail;
  for(i=0;i<3;i++){
    c->names[i]=sqlite3_mprintf("%s%s",name,i==1?"-wal":i==2?"-shm":"");
    if(!c->names[i]){ rc=SQLITE_NOMEM; goto fail; }
    if(i==0 && mode==1){
      c->files[0]=ryeosGateCreateAt(c,c->names[0]);
      if(c->files[0]<0) goto fail;
      c->createdMain=1;
      /* Retain the exclusive-created descriptor; no protected raw close. */
      c->pins[0]=fcntl(c->files[0],F_DUPFD_CLOEXEC,0);
      if(c->pins[0]<0 || fsync(c->files[0])!=0 || fsync(c->root)!=0) goto fail;
    }else c->pins[i]=openat(c->root,c->names[i],O_PATH|O_NOFOLLOW|O_CLOEXEC);
    if(c->pins[i]<0){
      if(errno!=ENOENT || i==0) goto fail;
    }else{
      char pinned[64];
      if(mode==1 && i>0) goto fail; /* refuse a raced orphan, never adopt it */
      if(osFstat(c->pins[i],&s)!=0 || !S_ISREG(s.st_mode)) goto fail;
      snprintf(pinned,sizeof(pinned),"/proc/self/fd/%d",c->pins[i]);
      if(c->files[i]<0) c->files[i]=robust_open(pinned,(c->readonlySource || (i==0 && c->readonlyMain) ? O_RDONLY : O_RDWR)|O_CLOEXEC,0);
      if(c->files[i]<0 || !ryeosSameFd(c->pins[i],c->files[i])) goto fail;
      ryeosGateOwner(c->files[i],c);
    }
    c->paths[i]=sqlite3_mprintf("/proc/self/fd/%d/%s",c->root,c->names[i]);
    if(!c->paths[i]){ rc=SQLITE_NOMEM; goto fail; }
  }
  if(ryeosCheck(c)!=SQLITE_OK) goto fail;
  c->vfs=*base;
  c->vfsName=sqlite3_mprintf("lillux-bound-%p",(void*)c);
  if(!c->vfsName){ rc=SQLITE_NOMEM; goto fail; }
  c->vfs.zName=c->vfsName; c->vfs.pNext=0;
  c->vfs.xOpen=ryeosBoundOpen; c->vfs.xFullPathname=ryeosBoundFullPath;
  c->vfs.xDelete=ryeosBoundDelete; c->vfs.xAccess=ryeosBoundAccess;
  rc=sqlite3_vfs_register(&c->vfs,0); if(rc!=SQLITE_OK) goto fail;
  c->vfsLinked=1;
  *out=c; return SQLITE_OK;
fail: *out=c; return rc; /* caller owns every partially acquired slot */
}
SQLITE_API int ryeos_sqlite_binding_created(void *h){
  if(!ryeos_native_process_matches()){return 0;} return ((RyeosBinding*)h)->createdMain; }
SQLITE_API const char *ryeos_sqlite_binding_name(void *h){
  if(!ryeos_native_process_matches()){return 0;} return ((RyeosBinding*)h)->vfsName; }
SQLITE_API const char *ryeos_sqlite_binding_path(void *h){
  if(!ryeos_native_process_matches()){return 0;} return ((RyeosBinding*)h)->paths[0]; }
/* Snapshot only the existing native main keeper. No descriptor is duplicated. */
SQLITE_API int ryeos_sqlite_binding_main_identity(void *h,sqlite3_uint64 *dev,sqlite3_uint64 *ino){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)h;struct stat st;
  if(!c || !dev || !ino || atomic_load(&c->poisoned)
      || osFstat(atomic_load(&c->files[0]),&st)!=0 || !S_ISREG(st.st_mode))return SQLITE_IOERR;
  *dev=(sqlite3_uint64)st.st_dev;*ino=(sqlite3_uint64)st.st_ino;return SQLITE_OK;
}
/* An existing main proves empty only while its retained keeper is a zero-byte
   regular file and no WAL role was found at acquisition: nothing then exists
   for a rollback journal to protect. No descriptor is duplicated. */
SQLITE_API int ryeos_sqlite_binding_main_empty(void *h,int *empty){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)h;struct stat st;
  if(!empty)return SQLITE_MISUSE;
  *empty=0;
  if(!c || atomic_load(&c->poisoned) || c->readonlyMain
      || osFstat(atomic_load(&c->files[0]),&st)!=0 || !S_ISREG(st.st_mode))return SQLITE_IOERR;
  *empty=st.st_size==0 && atomic_load(&c->files[1])<0 && atomic_load(&c->pins[1])<0;
  return SQLITE_OK;
}
/* Closed original proof owns these O_PATH pins. The alias gate spans every
   data-capable open, fsync and close; a live SQLite inode is refused first. */
SQLITE_API int ryeos_sqlite_sync_closed_main(int pin,int wal,sqlite3_uint64 dev,sqlite3_uint64 ino,int *synced,int *closeAttempted,int *closeError){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  struct stat st,peer,walst;RyeosGateFd *node;char exact[64];int fd=-1,rc=SQLITE_IOERR_FSYNC;
  if(!synced || !closeAttempted || !closeError)return SQLITE_MISUSE;
  *synced=0;*closeAttempted=0;*closeError=0;
  if(!(fcntl(pin,F_GETFL)&O_PATH) || osFstat(pin,&st)!=0 || !S_ISREG(st.st_mode)
      || (sqlite3_uint64)st.st_dev!=dev || (sqlite3_uint64)st.st_ino!=ino)return SQLITE_MISUSE;
  if(wal>=0 && (!(fcntl(wal,F_GETFL)&O_PATH) || osFstat(wal,&walst)!=0
      || !S_ISREG(walst.st_mode) || walst.st_size!=0))return SQLITE_BUSY;
  if(pthread_mutex_trylock(&ryeosGateMutex)!=0)return SQLITE_BUSY;
  for(node=ryeosGateFiles;node;node=node->next){
    if(osFstat(node->fd,&peer)!=0)goto done;
    if((peer.st_dev==st.st_dev && peer.st_ino==st.st_ino)
        || (wal>=0 && peer.st_dev==walst.st_dev && peer.st_ino==walst.st_ino)){rc=SQLITE_BUSY;goto done;}
  }
  if(ryeosLeaseBlocksFd(pin) || ryeosGateAdmissionClaim(pin)
      || (wal>=0 && (ryeosLeaseBlocksFd(wal) || ryeosGateAdmissionClaim(wal)))){rc=SQLITE_BUSY;goto done;}
  if(snprintf(exact,sizeof(exact),"/proc/self/fd/%d",pin)>=(int)sizeof(exact))goto done;
  fd=ryeosGateRawOpen(exact,O_RDWR|O_CLOEXEC|O_NONBLOCK,0);
  if(fd<0 || osFstat(fd,&peer)!=0 || !S_ISREG(peer.st_mode)
      || peer.st_dev!=st.st_dev || peer.st_ino!=st.st_ino)goto done;
  if(wal>=0 && (osFstat(wal,&peer)!=0 || peer.st_size!=0)){rc=SQLITE_BUSY;goto done;}
  if(fsync(fd)!=0)goto done;
  *synced=1; /* irreversible fact retained even if later close refuses */
  rc=SQLITE_OK;
done:
  if(fd>=0){
    *closeAttempted=1;
    if(close(fd)!=0){*closeError=errno;rc=SQLITE_IOERR_CLOSE;}
    /* Linux consumes this descriptor coordinate even on EINTR/late error.
       The original refusal is retained; this numeric FD is never retried. */
  }
  pthread_mutex_unlock(&ryeosGateMutex);return rc;
}
SQLITE_API int ryeos_sqlite_binding_sync_live(void *h,sqlite3 *db,int *walSynced,int *mainSynced){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosNativeArena *a=atomic_load(&ryeosNativeOwner);
  RyeosBinding *c=(RyeosBinding*)h;sqlite3_file *main=0,*wal=0;struct stat st;int rc;
  if(!walSynced || !mainSynced)return SQLITE_MISUSE;
  *walSynced=0;*mainSynced=0;
  if(!c || !db || !ryeosNativeContext(a) || ryeosNativeCleanup
      || !ryeosNativeCharge(a,1,0))return SQLITE_MISUSE;
  sqlite3_mutex_enter(sqlite3_db_mutex(db));
  rc=sqlite3_file_control(db,"main",SQLITE_FCNTL_FILE_POINTER,&main);
  if(rc!=SQLITE_OK || !main || main->pMethods!=&posixIoMethods
      || ((unixFile*)main)->pVfs!=&c->vfs || ryeosBoundWritable((unixFile*)main)!=SQLITE_OK){rc=SQLITE_IOERR;goto done;}
  rc=sqlite3_file_control(db,"main",SQLITE_FCNTL_JOURNAL_POINTER,&wal);
  if(rc!=SQLITE_OK || !wal || wal->pMethods!=&nolockIoMethods
      || ((unixFile*)wal)->pVfs!=&c->vfs || ryeosBoundWritable((unixFile*)wal)!=SQLITE_OK
      || osFstat(((unixFile*)wal)->h,&st)!=0 || !S_ISREG(st.st_mode) || st.st_size!=0){rc=SQLITE_BUSY;goto done;}
  unixEnterMutex();
  rc=ryeosBoundShm((unixFile*)main,((unixFile*)main)->pShm ? ((unixFile*)main)->pShm->pShmNode : 0);
  unixLeaveMutex();
  if(rc!=SQLITE_OK)goto done;
  if(!ryeosNativeCharge(a,1,0)){rc=SQLITE_NOMEM;goto done;}
  rc=wal->pMethods->xSync(wal,SQLITE_SYNC_FULL);
  if(rc!=SQLITE_OK)goto done;
  *walSynced=1;
  if(!ryeosNativeCharge(a,1,0)){rc=SQLITE_NOMEM;goto done;}
  rc=main->pMethods->xSync(main,SQLITE_SYNC_FULL);
  if(rc!=SQLITE_OK)goto done;
  *mainSynced=1;
  if(ryeosBoundWritable((unixFile*)main)!=SQLITE_OK
      || ryeosBoundWritable((unixFile*)wal)!=SQLITE_OK
      || osFstat(((unixFile*)wal)->h,&st)!=0 || st.st_size!=0)rc=SQLITE_IOERR;
done:
  if(rc!=SQLITE_OK && rc!=SQLITE_NOMEM)c->poisoned=1;
  sqlite3_mutex_leave(sqlite3_db_mutex(db));return rc;
}
SQLITE_API int ryeos_sqlite_binding_validate(void *h,sqlite3 *db,int all){
  if(!ryeos_native_process_matches()){return SQLITE_MISUSE;}
  RyeosBinding *c=(RyeosBinding*)h; sqlite3_file *main=0,*wal=0; int rc;
  sqlite3_mutex_enter(sqlite3_db_mutex(db));
  rc=sqlite3_file_control(db,"main",SQLITE_FCNTL_FILE_POINTER,&main);
  if(rc!=SQLITE_OK || !main || main->pMethods!=&posixIoMethods
      || ((unixFile*)main)->pVfs!=&c->vfs || ryeosBoundFile((unixFile*)main)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
  if(all){
    rc=sqlite3_file_control(db,"main",SQLITE_FCNTL_JOURNAL_POINTER,&wal);
    if(rc!=SQLITE_OK || !wal || wal->pMethods!=&nolockIoMethods
       || ((unixFile*)wal)->pVfs!=&c->vfs || ryeosBoundFile((unixFile*)wal)!=SQLITE_OK){ rc=SQLITE_IOERR; goto done; }
    unixEnterMutex();
    rc=ryeosBoundShm((unixFile*)main,((unixFile*)main)->pShm ? ((unixFile*)main)->pShm->pShmNode : 0);
    unixLeaveMutex();
  }
done: if(rc!=SQLITE_OK) c->poisoned=1;
  sqlite3_mutex_leave(sqlite3_db_mutex(db)); return rc;
}
#else
SQLITE_API int ryeos_native_process_matches(void){return 0;}

static void *ryeosNativeMalloc(int n){(void)n;return 0;}
static void ryeosNativeFree(void*p){(void)p;}
static void *ryeosNativeRealloc(void*p,int n){(void)p;(void)n;return 0;}
static int ryeosNativeSize(void*p){(void)p;return 0;}
static int ryeosNativeRoundup(int n){(void)n;return 0;}
static int ryeosNativeInit(void*p){(void)p;return SQLITE_MISUSE;}
static void ryeosNativeShutdown(void*p){(void)p;}
static int ryeosNativeInitGate(void){return 0;}
static int ryeosNativeShutdownGate(void){return 0;}
static int ryeosNativeOpenGate(void){return 0;}
static int ryeosNativeConfigGate(int op){(void)op;return 0;}
static int ryeosNativeCommitGate(sqlite3 *db){(void)db;return 0;}
static int ryeosNativeBackupGate(void){return 0;}
static int ryeosNativeInGateActive(void){return 0;}
#ifndef SQLITE_OMIT_WAL
static int ryeosNativeShmReserve(unixShmNode*n,int r,int s,int g,int*q){(void)n;(void)r;(void)s;(void)g;if(q)*q=0;return SQLITE_NOTFOUND;}
static void *ryeosNativeShmMap(unixShmNode*n,int b,int p,sqlite3_int64 o){(void)n;(void)b;(void)p;(void)o;return (void*)-1;}
static int ryeosNativeShmEnd(unixShmNode*n){(void)n;return SQLITE_NOTFOUND;}
static int ryeosNativeShmUnmap(unixShmNode*n,void*p){(void)n;(void)p;return SQLITE_NOTFOUND;}
#endif
SQLITE_API int ryeos_native_layout(sqlite3_uint64 s,unsigned int t,sqlite3_uint64*c){(void)s;(void)t;if(c)*c=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_arena_create(const void*b,void**o){(void)b;if(o)*o=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_scope_enter(void*a,int c,unsigned int t){(void)a;(void)c;(void)t;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_scope_leave(void*a){(void)a;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_arena_ready(void*a){(void)a;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_connection_enroll(void*a,sqlite3*d,unsigned int t){(void)a;(void)d;(void)t;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_connection_closed(void*a,unsigned int t){(void)a;(void)t;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_commit_verifier_install(void*a,int(*v)(unsigned int)){(void)a;(void)v;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_arena_reclaim(void*a){(void)a;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_arena_release(void*a){(void)a;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_native_shared_census(void*a,sqlite3_uint64*b,unsigned int*s){(void)a;(void)b;(void)s;return SQLITE_NOTFOUND;}
/* Never silently certify unsupported platforms or SQLite configurations. */
SQLITE_API int ryeos_sqlite_stopped_create(int r,const char *const *n,int count,const int *s,const sqlite3_uint64 *d,const sqlite3_uint64 *i,const sqlite3_uint64 *l,void **o){ (void)r;(void)n;(void)count;(void)s;(void)d;(void)i;(void)l;*o=0;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_stopped_acquire(void *h){ (void)h;return SQLITE_NOTFOUND; }

SQLITE_API int ryeos_sqlite_close_original_finalizer_file(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int kind,int *consumed,int *observed_errno){
  (void)fd;(void)dev;(void)ino;(void)kind;
  if(consumed)*consumed=0;
  if(observed_errno)*observed_errno=0;
  return SQLITE_NOTFOUND;
}
SQLITE_API int ryeos_sqlite_close_original_metadata_pin(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int *consumed,int *observed_errno){
  (void)fd;(void)dev;(void)ino;
  if(consumed)*consumed=0;
  if(observed_errno)*observed_errno=0;
  return SQLITE_NOTFOUND;
}
SQLITE_API int ryeos_sqlite_close_original_output(int fd,sqlite3_uint64 dev,sqlite3_uint64 ino,int maximum,int *consumed,int *observed_errno){
  (void)fd;(void)dev;(void)ino;(void)maximum;
  if(consumed) *consumed=0;
  if(observed_errno) *observed_errno=0;
  return SQLITE_NOTFOUND;
}
SQLITE_API int ryeos_sqlite_read_unbound_regular(int p,unsigned char *b,int n,int *w){ (void)p;(void)b;(void)n;if(w)*w=0;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_read_unbound_witness(int p,unsigned char *b,int n,int *w){ (void)p;(void)b;(void)n;if(w)*w=0;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_read_unbound_regular_chunk(int p,sqlite3_uint64 o,sqlite3_uint64 l,unsigned char *b,int n,int *w){ (void)p;(void)o;(void)l;(void)b;(void)n;if(w)*w=0;return SQLITE_NOTFOUND; }

SQLITE_API int ryeos_sqlite_retirement_capture(void *h,const char *g){ (void)h;(void)g;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_retirement_barriers(void *h){ (void)h;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_retirement_role_fd(void *h,int r){ (void)h;(void)r;return -1; }
SQLITE_API int ryeos_sqlite_retirement_allow_release(void *h){ (void)h;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_binding_create(int fd,const char *n,int mode,void *lease,void **out){ (void)fd;(void)n;(void)mode;(void)lease;*out=0;return SQLITE_NOTFOUND; }
SQLITE_API int ryeos_sqlite_binding_created(void *h){ (void)h;return 0; }
SQLITE_API void ryeos_sqlite_binding_drop(void *h){ (void)h; }
SQLITE_API void ryeos_sqlite_binding_abandon(void *h,sqlite3 *db){ (void)h;(void)db; }
SQLITE_API int ryeos_sqlite_binding_release(void *h,void **lease){ (void)h;*lease=0;return 2; }
SQLITE_API void *ryeos_sqlite_take_ready_copy_lease(void){ return 0; }
SQLITE_API const char *ryeos_sqlite_binding_name(void *h){ (void)h;return 0; }
SQLITE_API const char *ryeos_sqlite_binding_path(void *h){ (void)h;return 0; }
SQLITE_API int ryeos_sqlite_binding_main_identity(void*h,sqlite3_uint64*d,sqlite3_uint64*i){(void)h;(void)d;(void)i;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_sqlite_binding_main_empty(void*h,int*e){(void)h;if(e)*e=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_sqlite_sync_closed_main(int p,int w,sqlite3_uint64 d,sqlite3_uint64 i,int*s,int*a,int*e){(void)p;(void)w;(void)d;(void)i;if(s)*s=0;if(a)*a=0;if(e)*e=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_projection_lease_open(int p,int m,void**o,int*f){(void)p;(void)m;if(o)*o=0;if(f)*f=-1;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_projection_lease_close(void*h,int*p,int*f){(void)h;if(p)*p=0;if(f)*f=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_sqlite_binding_sync_live(void*h,sqlite3*d,int*w,int*m){(void)h;(void)d;if(w)*w=0;if(m)*m=0;return SQLITE_NOTFOUND;}
SQLITE_API int ryeos_sqlite_binding_validate(void *h,sqlite3 *db,int all){ (void)h;(void)db;(void)all;return SQLITE_NOTFOUND; }
#if SQLITE_OS_UNIX
static int robust_open(const char *p,int f,mode_t m){ return ryeosRawOpen(p,f,m); }
static int ryeosStockClose(int fd){ return osClose(fd)!=0; }
static int ryeosStockOpeningDenied(const char *p){ (void)p;return 0; }
static int ryeosIsBound(sqlite3_vfs *v){(void)v;return 0;}
static void ryeosReapKeepersLocked(void){}
static void *ryeosBoundClosing(unixFile*f){(void)f;return 0;}
static void ryeosBoundClosed(void*p){(void)p;}
static int ryeosBoundOpen(sqlite3_vfs*v,const char*p,sqlite3_file*f,int x,int*y){(void)v;(void)p;(void)f;(void)x;(void)y;return SQLITE_NOTFOUND;}
static int ryeosBoundFd(sqlite3_vfs*v,const char*p,int f,int x){(void)v;(void)p;(void)f;(void)x;return SQLITE_NOTFOUND;}
static int ryeosBoundOpenFd(sqlite3_vfs*v,const char*p,int f,int m){(void)v;return robust_open(p,f,m);}
static int ryeosBoundFile(unixFile*f){(void)f;return SQLITE_OK;}
static int ryeosBoundWritable(unixFile*f){(void)f;return SQLITE_NOTFOUND;}
static int ryeosBoundShm(unixFile*f,unixShmNode*n){(void)f;(void)n;return SQLITE_OK;}
static int ryeosBoundShmAdopt(unixFile*f,unixShmNode*n){(void)f;(void)n;return SQLITE_NOTFOUND;}
static int ryeosBoundShmOpen(unixFile*f,const char*p,int x,int m){(void)f;return robust_open(p,x,m);}
#endif
#endif
