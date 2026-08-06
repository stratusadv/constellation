//! A random sequence of filesystem operations, always converging.
//!
//! The value here is not any single generated case but the regressions file:
//! `tests/fuzz_file_operations.proptest-regressions` is committed, so a seed
//! that once broke convergence is replayed on every run forever after. Commit
//! that file whenever proptest writes to it; the seeds are the durable half of
//! this test.

use crate::common::{Workspace, module_source};

use proptest::prelude::*;

/// The bound on operations in one generated sequence. Each one is real
/// filesystem work followed by a convergence wait, so the bound is what keeps a
/// proptest run to minutes rather than hours.
const OPERATIONS_MAX: usize = 12;

/// The files a sequence may address, so operations collide often enough to
/// exercise create-after-delete and rewrite-after-rename.
const FILE_SLOTS: usize = 6;

/// A filesystem operation against a numbered slot.
#[derive(Clone, Debug)]
enum Operation {
    Create(usize),
    Delete(usize),
    Modify(usize),
    Rename(usize, usize),
}

/// The strategy generating one operation.
fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0..FILE_SLOTS).prop_map(Operation::Create),
        (0..FILE_SLOTS).prop_map(Operation::Delete),
        (0..FILE_SLOTS).prop_map(Operation::Modify),
        (0..FILE_SLOTS, 0..FILE_SLOTS).prop_map(|(from, to)| Operation::Rename(from, to)),
    ]
}

/// The path of one numbered slot.
fn slot_path(slot: usize) -> String {
    format!("app/slot{slot}.py")
}

/// An operation applied to the workspace.
fn apply(workspace: &Workspace, operation: &Operation) {
    match operation {
        Operation::Create(slot) => {
            workspace.write(&slot_path(*slot), &module_source(&format!("Created{slot}")));
        }
        Operation::Delete(slot) => workspace.remove(&slot_path(*slot)),
        Operation::Modify(slot) => {
            workspace.write(
                &slot_path(*slot),
                &format!("{}\n\n\ndef extra_{slot}():\n    return {slot}\n", module_source(&format!("Modified{slot}"))),
            );
        }
        Operation::Rename(from, to) => {
            if from != to {
                workspace.rename(&slot_path(*from), &slot_path(*to));
            }
        }
    }
}

proptest! {
    // Sized against measured wall clock, not guessed: each case is real
    // filesystem work plus a convergence wait, and this count keeps the file
    // under ten seconds while actually exploring the operation space. A handful
    // of cases would be a smoke test wearing a fuzzer's name.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_sequence_of_operations_converges_to_a_from_scratch_index(
        operations in prop::collection::vec(operation(), 1..OPERATIONS_MAX)
    ) {
        prop_assert!(operations.len() < OPERATIONS_MAX, "the sequence respects its bound");

        let workspace = Workspace::new("fuzz-operations");
        let _handle = workspace.watch();

        for operation in &operations {
            apply(&workspace, operation);
        }

        let converged = workspace.wait_for_convergence();

        prop_assert!(
            converged.is_converged(),
            "sequence {:?} left the store out of step with disk.\n{}",
            operations,
            converged.describe(),
        );
    }
}
