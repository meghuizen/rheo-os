//! In-QEMU test kernel: the four capability-core proof properties from
//! docs/ARCHITECTURE.md 8.2, exercised at runtime. These are the checks
//! the Verus proofs will eventually machine-verify (BUILD-ORDER.md step
//! 4); until then this suite is the falsification harness for the
//! security concept, and it must stay green on every commit.

#![no_std]
#![no_main]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use kernel::capability::typed::{Capability, Full, ReadOnly};
use kernel::capability::{
    BUDGET_UNLIMITED, CapError, DELEGATE, Handle, ObjectKind, ObjectTable, READ, WRITE,
};
use kernel::cell::Cell;
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("cap-invariants: start on {}", arch::NAME);

    let mut objects = ObjectTable::new();
    let mut cell_a = Cell::new(1);
    let mut cell_b = Cell::new(2);

    let object = objects.create(ObjectKind::MemoryGrant).unwrap();
    let root = cell_a
        .caps
        .mint(&objects, object, READ | WRITE | DELEGATE, BUDGET_UNLIMITED)
        .unwrap();

    // ---- Property 1: unforgeability -----------------------------------
    // A handle is a per-cell table index, not a secret: inside your own
    // table a guessed value can only reach grants you already hold, with
    // their rights still enforced. Unforgeability means a cell that holds
    // nothing can fabricate nothing - so the sweep runs against cell B,
    // whose table is empty.
    for raw in [0u64, 1, 0xFFFF, 0x0001_0000, 0xDEAD_BEEF, u64::MAX] {
        assert!(
            cell_b
                .caps
                .grant_check(&objects, Handle::forge(raw), READ)
                .is_err(),
            "forged handle {raw:#x} passed the grant check in an empty cell"
        );
    }
    // Guessing within your own table cannot escalate rights either.
    assert_eq!(
        cell_a.caps.grant_check(
            &objects,
            Handle::forge(0x0001_0000),
            READ | WRITE | DELEGATE | 0x20
        ),
        Err(CapError::InsufficientRights),
        "a guessed handle escalated rights"
    );
    // A freed handle is stale, and the reused slot gets a new generation.
    let temp = cell_a
        .caps
        .derive_subset(&objects, root, READ, BUDGET_UNLIMITED)
        .unwrap();
    cell_a.caps.free(temp);
    assert_eq!(
        cell_a.caps.grant_check(&objects, temp, READ),
        Err(CapError::BadHandle),
        "freed handle still passes"
    );
    let reused = cell_a
        .caps
        .derive_subset(&objects, root, READ, BUDGET_UNLIMITED)
        .unwrap();
    assert_ne!(reused, temp, "slot reuse produced an identical handle");
    assert!(cell_a.caps.grant_check(&objects, temp, READ).is_err());
    println!("cap-invariants: 1 unforgeability OK");

    // ---- Property 2: monotonic attenuation ----------------------------
    // Delegation and derivation never widen rights.
    let read_only = cell_a
        .caps
        .derive_subset(&objects, root, READ, BUDGET_UNLIMITED)
        .unwrap();
    assert_eq!(
        cell_a
            .caps
            .derive_subset(&objects, read_only, READ | WRITE, BUDGET_UNLIMITED)
            .err(),
        Some(CapError::WidenAttempt),
        "derive widened READ to READ|WRITE"
    );
    // The grant check enforces the narrowed rights at use time too.
    assert_eq!(
        cell_a.caps.grant_check(&objects, read_only, WRITE),
        Err(CapError::InsufficientRights)
    );
    // And the typed layer makes widening a *compile* error - this line
    // does not build (SubsetOf cannot be satisfied), which is the test:
    //   let widened: Full<Marker> = narrow.attenuate();
    println!("cap-invariants: 2 monotonic attenuation OK");

    // ---- Property 3: revocation soundness ------------------------------
    // After the object's epoch is bumped, every capability from the old
    // epoch fails; a fresh mint under the new epoch works.
    let derived = cell_a
        .caps
        .derive_subset(&objects, root, READ, BUDGET_UNLIMITED)
        .unwrap();
    objects.revoke_epoch(object);
    for (name, handle) in [("root", root), ("derived", derived)] {
        assert_eq!(
            cell_a.caps.grant_check(&objects, handle, READ),
            Err(CapError::Revoked),
            "{name} survived revocation"
        );
    }
    let root2 = cell_a
        .caps
        .mint(&objects, object, READ | WRITE | DELEGATE, BUDGET_UNLIMITED)
        .unwrap();
    assert!(cell_a.caps.grant_check(&objects, root2, READ).is_ok());
    println!("cap-invariants: 3 revocation soundness OK");

    // ---- Property 4: isolation ----------------------------------------
    // A cell reaches an object only through its own table. Delegation is
    // a move: the source's handle dies, the target's works.
    assert_eq!(cell_b.caps.live_count(), 0);
    let for_b = cell_a
        .caps
        .derive_subset(&objects, root2, READ | DELEGATE, BUDGET_UNLIMITED)
        .unwrap();
    let b_handle = cell_a
        .caps
        .delegate(&objects, for_b, &mut cell_b.caps)
        .unwrap();
    assert!(cell_b.caps.grant_check(&objects, b_handle, READ).is_ok());
    assert!(
        cell_a.caps.grant_check(&objects, for_b, READ).is_err(),
        "delegated capability still usable in the source cell"
    );
    // Without DELEGATE the move is refused.
    let undelegatable = cell_a
        .caps
        .derive_subset(&objects, root2, READ, BUDGET_UNLIMITED)
        .unwrap();
    assert_eq!(
        cell_a
            .caps
            .delegate(&objects, undelegatable, &mut cell_b.caps)
            .err(),
        Some(CapError::NotDelegatable)
    );
    println!("cap-invariants: 4 isolation OK");

    // ---- Budget metering (capabilities are budget-metered, obj. 2) ----
    let metered = cell_a.caps.derive_subset(&objects, root2, READ, 3).unwrap();
    for _ in 0..3 {
        assert!(cell_a.caps.grant_check(&objects, metered, READ).is_ok());
    }
    assert_eq!(
        cell_a.caps.grant_check(&objects, metered, READ),
        Err(CapError::Exhausted)
    );
    println!("cap-invariants: budget metering OK");

    // ---- Typed layer (docs/KERNEL-RUST.md 2) ---------------------------
    struct Marker;
    let object2 = objects.create(ObjectKind::MemoryGrant).unwrap();
    let full: Full<Marker> = Capability::mint(&mut cell_a.caps, &objects, object2).unwrap();
    let narrow: ReadOnly<Marker> = full.attenuate();
    assert!(narrow.grant_check(&mut cell_a.caps, &objects).is_ok());
    println!("cap-invariants: typed rights layer OK");

    // ---- The table has no fixed ceiling (docs/EXECUTION-MODEL.md 9.7) -------
    //
    // `MAX_CAPS_PER_CELL = 256` was a `[CapSlot; 256]` per cell - 131,072 bytes of `.bss`
    // at 16 cells, by a wide margin the largest fixed table left, and a hard cap on how
    // many objects one cell could ever reach. It is an inline half plus a tail funded from
    // the cell's own budget now.
    //
    // Minting **past the old ceiling** is the claim, so the number is deliberately larger
    // than 256: at 300 the old table returned `TableFull` and this could not have run at
    // all. The frame side is asserted too - the table must actually have grown - because
    // an inline half quietly enlarged to 300 would pass the mint loop and prove nothing.
    {
        let owner = kernel::mm::kmeta::Owner::cell(9);
        cell_a.caps.clear();
        cell_a.caps.set_owner(owner);
        let obj = objects.create(ObjectKind::MemoryGrant).unwrap();
        let mut held = 0usize;
        for _ in 0..300 {
            match cell_a.caps.mint(&objects, obj, READ, 0) {
                Ok(_) => held += 1,
                Err(e) => panic!("mint {held} of 300 failed: {e:?}"),
            }
        }
        assert_eq!(held, 300, "only {held} of 300 capabilities were minted");
        assert_eq!(
            cell_a.caps.live_count(),
            300,
            "the table reports {} live capabilities, want 300",
            cell_a.caps.live_count()
        );
        let frames = cell_a.caps.frames_held();
        assert!(
            frames > 0,
            "300 capabilities in one table cost 0 frames - the inline half absorbed them \
             all, so this is still a fixed array and the funded path never ran"
        );
        // Charged to the cell that caused the growth, which is what makes exhaustion
        // attributable instead of a global "table full" (docs/SUBSTRATE.md pillar 1).
        assert_eq!(
            kernel::mm::kmeta::charged(owner),
            frames,
            "the table holds {frames} frame(s) but the owner is charged {}",
            kernel::mm::kmeta::charged(owner)
        );
        // **The charge follows the table when it changes hands.** This is the case the
        // conversion was held back on: a capability table is built by whatever launches a
        // cell and adopted by the cell at `install`, so `set_owner` can land on a table
        // that has *already grown*. `Funded::release` credits the owner recorded at
        // release time, so relabelling instead of transferring would credit a ledger that
        // was never charged - exhaustion attributed to the wrong cell, which is the one
        // thing the per-owner ledger exists to get right (docs/EXECUTION-MODEL.md 9.7).
        //
        // Asserted here rather than left to the install path, because `set_owner` on an
        // *empty* table has nothing to move and passes either way - which is exactly what
        // the first version of this control did.
        let adopter = kernel::mm::kmeta::Owner::cell(10);
        cell_a.caps.set_owner(adopter);
        assert_eq!(
            kernel::mm::kmeta::charged(owner),
            0,
            "the previous owner is still charged {} frame(s) after the table was adopted",
            kernel::mm::kmeta::charged(owner)
        );
        assert_eq!(
            kernel::mm::kmeta::charged(adopter),
            frames,
            "the adopting owner is charged {} of the table's {frames} frame(s)",
            kernel::mm::kmeta::charged(adopter)
        );

        cell_a.caps.release();
        assert_eq!(
            kernel::mm::kmeta::charged(adopter),
            0,
            "the owner is still charged after the table was released - a table handed back \
             without releasing its frames is the S1' leak"
        );
        println!(
            "cap-invariants: THE CAPABILITY TABLE HAS NO CEILING - 300 capabilities in one \
             cell, past the old `MAX_CAPS_PER_CELL` of 256 where this returned TableFull, \
             costing {frames} frame(s) charged to that cell, MOVED to the cell that \
             adopted the table (the launcher-builds/cell-adopts lifecycle this conversion \
             was held back on), and all returned on release. It was a fixed \
             [CapSlot; 256] per cell = 131,072 bytes of .bss OK"
        );
    }

    println!("cap-invariants: PASS");
    arch::exit(arch::ExitCode::Success)
}
