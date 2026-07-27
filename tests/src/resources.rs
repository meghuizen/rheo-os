//! In-QEMU test kernel for the resource object model (docs/ARCHITECTURE.md
//! 3): clock/entropy, event streams, memory grants, reservations, leases,
//! the dependency graph, and the compute engine. Each check exercises a
//! real operation of the object; green on all three ISAs.

#![no_std]
#![no_main]

use kernel::engine::{Engine, Op};
use kernel::event::{self, EventStream};
use kernel::graph::{Graph, GraphError, Input};
use kernel::lease::{FencedResource, Lease, LeaseError};
use kernel::mm::grant::{Grant, GrantError, MemKind};
use kernel::rng::Drbg;
use kernel::sched::{Admission, AdmitError};
use kernel::time;
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("resources: start on {}", arch::NAME);

    test_time();
    test_events();
    test_grant();
    test_reservation();
    test_lease();
    test_engine_and_graph();

    println!("resources: PASS");
    arch::exit(arch::ExitCode::Success)
}

fn test_time() {
    // Monotonic counter never goes backwards; uptime advances.
    let a = time::monotonic();
    let b = time::monotonic();
    assert!(b >= a, "monotonic went backwards");
    let u = time::uptime_ticks();
    assert!(time::uptime_ticks() >= u, "uptime went backwards");
    // Wall reading is a bounded interval, not a bare instant; unsynced,
    // so it must report full uncertainty (error covers the whole reading).
    let w = time::wall();
    assert!(
        w.error >= w.center,
        "unsynced wall clock understated its error"
    );

    // Two derived DRBGs are independent and non-trivial.
    let mut d1 = Drbg::from_seed(1);
    let mut d2 = Drbg::from_seed(2);
    let (x1, x2) = (d1.next_u64(), d1.next_u64());
    assert!(x1 != x2, "DRBG produced a repeat");
    assert!(d2.next_u64() != x1, "distinct seeds collided");
    let mut child = d1.derive();
    assert!(
        child.next_u64() != d1.next_u64(),
        "child stream not independent"
    );
    println!("resources: clock + entropy OK");
}

fn test_events() {
    let mut s = EventStream::new();
    s.emit(event::EV_CELL_SPAWN, 0x1111, 1);
    s.emit(event::EV_QUEUE_SUBMIT, 0x2222, 2);
    assert_eq!(s.buffered(), 2);
    assert_eq!(s.total(), 2);
    let first = s.drain_one().unwrap();
    assert_eq!(first.kind, event::EV_CELL_SPAWN);
    assert_eq!(first.flow_id, 0x1111, "flow context not preserved");
    assert!(first.tick > 0, "event not timestamped");
    assert_eq!(s.drain_one().unwrap().kind, event::EV_QUEUE_SUBMIT);
    assert!(s.drain_one().is_none());

    // Overflow drops the oldest and counts it (hot path never blocks).
    let mut s2 = EventStream::new();
    for i in 0..(event::STREAM_CAP as u64 + 8) {
        s2.emit(event::EV_USER, 0, i);
    }
    assert_eq!(s2.buffered(), event::STREAM_CAP);
    assert_eq!(s2.dropped(), 8);
    println!("resources: event stream OK");
}

fn test_grant() {
    let (free_before, _) = kernel::mm::frames::stats();
    let mut g = Grant::new(MemKind::Ddr, true);
    g.commit(4).unwrap();
    assert_eq!(g.committed_pages(), 4);
    let (free_mid, _) = kernel::mm::frames::stats();
    assert_eq!(free_before - free_mid, 4, "commit did not take 4 frames");
    assert!(g.page(0).is_ok() && g.page(4).is_err());

    g.decommit(2).unwrap();
    assert_eq!(g.committed_pages(), 2);

    g.seal();
    assert_eq!(g.commit(1), Err(GrantError::Sealed), "sealed grant grew");
    assert!(g.is_sealed());
    drop(g);
    let (free_after, _) = kernel::mm::frames::stats();
    assert_eq!(free_after, free_before, "grant leaked frames on drop");
    println!("resources: memory grant OK");
}

fn test_reservation() {
    let mut adm = Admission::new();
    // 30% + 50% admit; the next 30% would overcommit and is refused.
    let r1 = adm.admit(3, 10, 10).unwrap();
    let r2 = adm.admit(5, 10, 10).unwrap();
    assert_eq!(adm.committed_ppm(), 800_000);
    assert_eq!(adm.admit(3, 10, 10).err(), Some(AdmitError::Overcommit));
    // Bad params refused up front.
    assert_eq!(adm.admit(11, 10, 10).err(), Some(AdmitError::BadParams));
    // Releasing frees utilization so a later reservation fits.
    adm.release(&r2);
    assert_eq!(adm.committed_ppm(), 300_000);
    adm.admit(5, 10, 10).unwrap();
    let _ = r1;
    println!("resources: reservation (EDF admission) OK");
}

fn test_lease() {
    let l1 = Lease::acquire(1 << 40, 0);
    let l2 = Lease::acquire(1 << 40, 0);
    assert!(l2.token > l1.token, "fencing tokens not increasing");

    let mut res = FencedResource::new();
    assert!(res.act(&l2).is_ok());
    // A stale (lower-token) holder is fenced at the resource.
    assert_eq!(res.act(&l1), Err(LeaseError::Fenced));

    // An expired lease is rejected.
    let expired = Lease::acquire(0, 0);
    let mut res2 = FencedResource::new();
    assert_eq!(res2.act(&expired), Err(LeaseError::Expired));

    // Epoch revocation invalidates leases from the old epoch.
    res.revoke_epoch();
    let l3 = Lease::acquire(1 << 40, 0); // old epoch
    assert_eq!(res.act(&l3), Err(LeaseError::Expired));
    let l4 = Lease::acquire(1 << 40, res.epoch());
    assert!(res.act(&l4).is_ok());
    println!("resources: lease (fencing + revocation) OK");
}

fn test_engine_and_graph() {
    let mut engine = Engine::cpu();
    engine.attach();
    assert!(engine.is_attached());
    assert_eq!(engine.exec(Op::Add, 20, 22), 42);
    assert_eq!(engine.exec(Op::Mul, 6, 7), 42);
    assert_eq!(engine.exec(Op::Select, 1, 99), 99);
    assert_eq!(engine.exec(Op::Select, 0, 99), 0);

    // A dependency graph = a pipeline submitted to the kernel: run
    // n0=const(6); n1=n0+1; n2=n1*n0  ->  (6+1)*6 = 42.
    let mut g = Graph::new();
    let n0 = g.push(Op::Const(6), Input::Imm(0), Input::Imm(0)).unwrap();
    let n1 = g.push(Op::Add, Input::Node(n0), Input::Imm(1)).unwrap();
    let _ = g.push(Op::Mul, Input::Node(n1), Input::Node(n0)).unwrap();
    let mut results = [0u64; kernel::graph::MAX_NODES];
    assert_eq!(g.run(&engine, &mut results), 42);

    // An edge to a not-yet-defined node is rejected (topological build).
    let mut bad = Graph::new();
    assert_eq!(
        bad.push(Op::Add, Input::Node(5), Input::Imm(0)),
        Err(GraphError::BadEdge)
    );
    println!("resources: engine + dependency graph OK");
}
