//! In-QEMU test kernel for the strand runtime (docs/BUILD-ORDER.md step 7,
//! docs/CONCURRENCY.md): the heap that backs `alloc`, the async executor,
//! an async channel, type-level capability rights, and - the headline -
//! native async running on the real queue-pair ABI: strands that park on a
//! submission's token and are woken by the completion carrying that token.
//!
//! Runs kernel-context so the model is proven correct on all three ISAs; the
//! same library links into a U-mode cell (that integration is future work,
//! it needs the .user heap grant + mem* shims the shell already hints at).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use kernel::capability::{BUDGET_UNLIMITED, ObjectKind, ObjectTable, READ, WRITE};
use kernel::cell::Cell;
use kernel::queue::{OP_NOP, QueuePair, kernel_process};
use kernel::{arch, println};
use runtime::rights::{
    Cap, EXECUTE as R_EXECUTE, Full, READ as R_READ, ReadOnly, ReadWrite, WRITE as R_WRITE,
};
use runtime::{Mutex, TicketLock, channel};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 1024 * 1024] = [0; 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("runtime: start on {}", arch::NAME);

    // SAFETY: called once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 1024 * 1024);
    }

    test_alloc();
    test_rights();
    test_executor_basic();
    test_park_complete();
    test_yield();
    test_join();
    test_channel();
    test_mutex();
    test_ticket_lock();
    test_async_on_queue();

    println!("runtime: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// `alloc` works on the OS heap: growable collections, boxing, a map, and
/// heap-formatted strings all round-trip.
fn test_alloc() {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1000 {
        v.push(i);
    }
    let sum: u64 = v.iter().sum();
    assert_eq!(sum, 499_500, "Vec sum wrong");
    v.retain(|x| x % 2 == 0);
    assert_eq!(v.len(), 500, "Vec retain wrong");

    let b = Box::new([7u8; 4096]);
    assert_eq!(b[4095], 7, "Box wrong");

    let mut m: BTreeMap<u64, u64> = BTreeMap::new();
    for i in 0..256 {
        m.insert(i, i * i);
    }
    assert_eq!(m.get(&16), Some(&256), "BTreeMap wrong");

    let mut s = String::new();
    write!(s, "n={}", 42).unwrap();
    assert_eq!(s, "n=42", "String fmt wrong");

    println!("runtime: alloc (Vec/Box/BTreeMap/String) OK");
}

/// Capability rights at the type level: narrowing type-checks and the mask
/// tracks it; widening would be a compile error (shown commented).
fn test_rights() {
    let rw: Cap<u64, ReadWrite> = Cap::from_handle(0x42);
    assert!(rw.allows(R_READ) && rw.allows(R_WRITE), "rw rights wrong");

    let ro: Cap<u64, ReadOnly> = rw.attenuate::<ReadOnly>();
    assert!(ro.allows(R_READ), "ro lost read");
    assert!(!ro.allows(R_WRITE), "ro kept write");
    assert_eq!(ro.handle(), 0x42, "attenuate changed the handle");

    // Widening does not type-check (SubsetOf is not satisfied):
    //   let _bad = ro.attenuate::<ReadWrite>();
    //   error: the trait bound `Assert<{...}>: IsTrue` is not satisfied

    let full: Cap<u64, Full> = Cap::from_handle(1);
    let _exec: Cap<u64, runtime::rights::Executable> = full.attenuate();
    assert!(!ro.allows(R_EXECUTE), "ro should not allow execute");

    println!("runtime: type-level rights (attenuation) OK");
}

/// The executor runs every spawned strand to completion.
fn test_executor_basic() {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    runtime::reset();
    COUNTER.store(0, Ordering::Relaxed);
    for _ in 0..10 {
        runtime::spawn(async {
            COUNTER.fetch_add(1, Ordering::Relaxed);
        });
    }
    runtime::run();
    assert!(!runtime::has_pending(), "strands left pending");
    let (spawned, finished) = runtime::stats();
    assert_eq!((spawned, finished), (10, 10), "spawn/finish counts wrong");
    assert_eq!(COUNTER.load(Ordering::Relaxed), 10, "not all strands ran");
    println!("runtime: executor spawn/run (10 strands) OK");
}

/// Park/complete ordering: strand A parks on a token; strand B runs, wakes
/// it. The recorded order must be B-then-A.
fn test_park_complete() {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    fn note(b: u8) {
        let v = SEQ.load(Ordering::Relaxed);
        SEQ.store((v << 8) | b as u32, Ordering::Relaxed);
    }
    runtime::reset();
    SEQ.store(0, Ordering::Relaxed);
    let token = runtime::next_token();
    runtime::spawn(async move {
        runtime::park_on(token).await;
        note(b'A');
    });
    runtime::spawn(async move {
        note(b'B');
        runtime::complete(token);
    });
    runtime::run();
    assert!(
        !runtime::has_pending(),
        "park/complete left a strand pending"
    );
    assert_eq!(
        SEQ.load(Ordering::Relaxed),
        0x4241,
        "order was not B then A"
    );
    println!("runtime: park/complete wake ordering OK");
}

/// yield_now cooperatively hands the vcore to the other ready strand, so two
/// strands interleave: a, b, then A, B.
fn test_yield() {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    fn note(b: u8) {
        let v = SEQ.load(Ordering::Relaxed);
        SEQ.store((v << 8) | b as u32, Ordering::Relaxed);
    }
    runtime::reset();
    SEQ.store(0, Ordering::Relaxed);
    runtime::spawn(async {
        note(b'a');
        runtime::yield_now().await;
        note(b'A');
    });
    runtime::spawn(async {
        note(b'b');
        runtime::yield_now().await;
        note(b'B');
    });
    runtime::run();
    assert!(!runtime::has_pending(), "yield left a strand pending");
    // 'a''b''A''B' = 0x61_62_41_42
    assert_eq!(
        SEQ.load(Ordering::Relaxed),
        0x6162_4142,
        "yield did not interleave"
    );
    println!("runtime: yield_now cooperative interleave OK");
}

/// Structured concurrency: a parent strand spawns children and joins their
/// typed results (one joined before it finishes, one after).
fn test_join() {
    static SUM: AtomicU64 = AtomicU64::new(0);
    runtime::reset();
    SUM.store(0, Ordering::Relaxed);
    runtime::spawn(async {
        let h1 = runtime::spawn(async { 3u64 });
        let h2 = runtime::spawn(async { 4u64 });
        let total = h1.join().await + h2.join().await;
        SUM.store(total, Ordering::Relaxed);
    });
    runtime::run();
    assert!(!runtime::has_pending(), "join left a strand pending");
    assert_eq!(SUM.load(Ordering::Relaxed), 7, "join produced wrong total");
    println!("runtime: spawn + typed join (structured concurrency) OK");
}

/// The async mutex serialises a read-modify-write that yields *inside* the
/// critical section. Without the lock the interleaved yield would lose
/// updates; with it every increment lands, so the total is exactly N.
fn test_mutex() {
    static RESULT: AtomicU64 = AtomicU64::new(0);
    const N: u64 = 16;
    runtime::reset();
    RESULT.store(0, Ordering::Relaxed);
    let m = Mutex::new(0u64);
    for _ in 0..N {
        let m2 = m.clone();
        runtime::spawn(async move {
            let mut g = m2.lock().await;
            let v = *g;
            // Yield while holding the lock: other strands must park on lock().
            runtime::yield_now().await;
            *g = v + 1;
        });
    }
    // Reader acquires after the writers and publishes the total.
    let mr = m.clone();
    runtime::spawn(async move {
        let g = mr.lock().await;
        RESULT.store(*g, Ordering::Relaxed);
    });
    runtime::run();
    assert!(!runtime::has_pending(), "mutex left a strand pending");
    assert_eq!(RESULT.load(Ordering::Relaxed), N, "mutex lost updates");
    println!("runtime: async mutex (park on contention, no lost updates) OK");
}

/// The ticket lock's algorithm, exercised uncontended (single owner): fair
/// acquire/release round-trips correctly. (Contended use is for multi-vcore.)
fn test_ticket_lock() {
    let lock = TicketLock::new(0u64);
    for _ in 0..100 {
        let mut g = lock.lock();
        *g += 1;
    }
    let g = lock.lock();
    assert_eq!(*g, 100, "ticket lock miscounted");
    println!("runtime: ticket lock (fair, uncontended) OK");
}

/// An async channel across two strands: the receiver parks empty, the sender
/// fills it and the receiver drains, summing to the expected total.
fn test_channel() {
    static SUM: AtomicU64 = AtomicU64::new(0);
    runtime::reset();
    SUM.store(0, Ordering::Relaxed);
    let (tx, rx) = channel::channel::<u64>();
    // Spawn the consumer first so it parks on the empty channel.
    runtime::spawn(async move {
        while let Some(v) = rx.recv().await {
            SUM.fetch_add(v, Ordering::Relaxed);
        }
    });
    runtime::spawn(async move {
        for i in 1..=100u64 {
            tx.send(i);
        }
        drop(tx); // closes the channel so recv() returns None
    });
    runtime::run();
    assert!(!runtime::has_pending(), "channel left a strand pending");
    assert_eq!(SUM.load(Ordering::Relaxed), 5050, "channel sum wrong");
    println!("runtime: async channel (park on empty, wake on send) OK");
}

// ---- native async on the real queue-pair ABI ----

/// The shared queue-pair region (header + SQ + CQ), page-aligned.
#[repr(C, align(4096))]
struct Region([u8; QueuePair::REGION_SIZE]);
static mut REGION: Region = Region([0; QueuePair::REGION_SIZE]);
static mut QP: Option<QueuePair> = None;

/// The headline: strands do I/O over the real queue-pair. Each strand submits
/// an op tagged with its own token (the SqEntry `user_data`) and parks; the
/// reactor drains the kernel completions and wakes each strand by the token
/// the completion carries. This is the CONCURRENCY.md 1 model end to end.
fn test_async_on_queue() {
    static DONE: AtomicU64 = AtomicU64::new(0);
    const N: u64 = 8;

    let mut objects = ObjectTable::new();
    let mut cell = Cell::new(1);
    let object = objects.create(ObjectKind::MemoryGrant).unwrap();
    let cap = cell
        .caps
        .mint(&objects, object, READ | WRITE, BUDGET_UNLIMITED)
        .unwrap();
    let abi_cap = cap.raw_low32();

    // SAFETY: single-CPU init of the queue-pair statics; the ref lives for
    // the rest of the run.
    let qp: &'static QueuePair = unsafe {
        *addr_of_mut!(QP) = Some(QueuePair::init(addr_of_mut!(REGION) as *mut u8));
        (*addr_of_mut!(QP)).as_ref().unwrap()
    };

    runtime::reset();
    DONE.store(0, Ordering::Relaxed);
    for _ in 0..N {
        runtime::spawn(async move {
            let token = runtime::next_token();
            // Submit an op carrying our token, then park until it completes.
            qp.submit(OP_NOP, abi_cap, 0, token);
            runtime::park_on(token).await;
            DONE.fetch_add(1, Ordering::Relaxed);
        });
    }

    // Reactor: run ready strands, then service the queue and wake by token.
    let mut guard = 0;
    while runtime::has_pending() {
        runtime::run();
        if !runtime::has_pending() {
            break;
        }
        kernel_process(qp, &mut cell.caps, &objects);
        let mut woke = 0;
        while let Some(cqe) = qp.cq.pop() {
            runtime::complete(cqe.user_data);
            woke += 1;
        }
        guard += 1;
        assert!(woke > 0, "reactor made no progress (deadlock)");
        assert!(guard < 100, "reactor ran away");
    }

    assert_eq!(
        DONE.load(Ordering::Relaxed),
        N,
        "not all queue strands woke"
    );
    println!("runtime: native async on the queue-pair ABI ({N} strands) OK");
}
