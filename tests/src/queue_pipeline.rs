//! In-QEMU test kernel: the queue-pair ABI as a use case - a cell submits
//! a batch of typed entries with one doorbell (doorbell coalescing,
//! docs/IO.md 1), the kernel grant-checks each entry, executes it, and
//! completes it with the 16-byte flow context preserved end to end
//! (docs/OBSERVABILITY.md: tracing the system cannot fail to produce).
//! Then the capability is revoked mid-stream and the data path must go
//! dark immediately - revocation enforced on the hot path, not at setup.

#![no_std]
#![no_main]

use kernel::capability::{BUDGET_UNLIMITED, ObjectKind, ObjectTable, READ, WRITE};
use kernel::cell::Cell;
use kernel::queue::{
    self, CqEntry, OP_ECHO, QueuePair, RING_DEPTH, STATUS_DENIED, STATUS_OK, SqEntry,
};
use kernel::{arch, println};

static mut SQ_STORAGE: [SqEntry; RING_DEPTH] = [SqEntry::ZERO; RING_DEPTH];
static mut CQ_STORAGE: [CqEntry; RING_DEPTH] = [CqEntry::ZERO; RING_DEPTH];

const BATCH: usize = 16;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("queue-pipeline: start on {}", arch::NAME);

    let mut objects = ObjectTable::new();
    let mut producer = Cell::new(1);

    let qp_object = objects.create(ObjectKind::QueuePair).unwrap();
    let cap = producer
        .caps
        .mint(&objects, qp_object, READ | WRITE, BUDGET_UNLIMITED)
        .unwrap();

    let qp = unsafe {
        QueuePair::new(
            core::ptr::addr_of_mut!(SQ_STORAGE) as *mut SqEntry,
            core::ptr::addr_of_mut!(CQ_STORAGE) as *mut CqEntry,
        )
    };

    // Submit a batch of echo ops, each with its own flow id; ring the
    // doorbell once for the whole batch.
    for i in 0..BATCH {
        let mut entry = SqEntry::new(
            OP_ECHO,
            cap,
            0x1000_0000_0000_0000_u128 + i as u128, // flow id
            i as u64,                               // user data
        );
        entry.payload[..4].copy_from_slice(&(0xA000_0000_u32 + i as u32).to_le_bytes());
        assert!(qp.sq.push(entry), "submission ring full");
    }
    arch::doorbell_trap();
    let processed = queue::kernel_process(&qp, &mut producer.caps, &objects);
    assert_eq!(processed, BATCH);

    // Every completion arrives, in order, with flow context and user data
    // intact and the echoed payload correct.
    for i in 0..BATCH {
        let completion = qp.cq.pop().expect("missing completion");
        assert_eq!(completion.status, STATUS_OK);
        assert_eq!(completion.flow_id, 0x1000_0000_0000_0000_u128 + i as u128);
        assert_eq!(completion.user_data, i as u64);
        assert_eq!(completion.result, 0xA000_0000 + i as u32);
    }
    assert!(qp.cq.pop().is_none());
    println!("queue-pipeline: batched submit + flow-context propagation OK");

    // A forged cap_id in an otherwise valid entry is denied per entry.
    let mut forged = SqEntry::new(OP_ECHO, cap, 0x77, 0x77);
    forged.cap_id = 0xFFFF_1234;
    qp.sq.push(forged);
    queue::kernel_process(&qp, &mut producer.caps, &objects);
    assert_eq!(qp.cq.pop().unwrap().status, STATUS_DENIED);
    println!("queue-pipeline: forged capability denied on data path OK");

    // Revoke mid-stream: the same capability that just worked goes dark.
    objects.revoke_epoch(qp_object);
    qp.sq.push(SqEntry::new(OP_ECHO, cap, 0x88, 0x88));
    queue::kernel_process(&qp, &mut producer.caps, &objects);
    let denied = qp.cq.pop().unwrap();
    assert_eq!(denied.status, STATUS_DENIED);
    assert_eq!(denied.flow_id, 0x88, "flow context lost on the deny path");
    println!("queue-pipeline: mid-stream revocation enforced OK");

    println!("queue-pipeline: PASS");
    arch::exit(arch::ExitCode::Success)
}
