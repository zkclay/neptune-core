use serde::{Deserialize, Serialize};

use twenty_first::shared_math::b_field_element::BFieldElement;
use twenty_first::shared_math::tip5::Digest;
use twenty_first::util_types::algebraic_hasher::Hashable;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdditionRecord {
    pub canonical_commitment: Digest,
}

impl AdditionRecord {
    pub fn new(canonical_commitment: Digest) -> Self {
        Self {
            canonical_commitment,
        }
    }
}

impl Hashable for AdditionRecord {
    fn to_sequence(&self) -> Vec<BFieldElement> {
        self.canonical_commitment.values().to_vec()
    }
}

#[cfg(test)]
mod addition_record_tests {
    use crate::util_types::mutator_set::mutator_set_trait::commit;

    use twenty_first::shared_math::tip5::Tip5;
    use twenty_first::util_types::algebraic_hasher::AlgebraicHasher;

    use super::*;

    #[test]
    fn hash_identity_test() {
        type H = Tip5;

        let addition_record_0: AdditionRecord = commit::<H>(
            &H::hash(&1492u128),
            &H::hash(&1522u128),
            &H::hash(&1521u128),
        );

        let addition_record_1: AdditionRecord = commit::<H>(
            &H::hash(&1492u128),
            &H::hash(&1522u128),
            &H::hash(&1521u128),
        );

        assert_eq!(
            H::hash(&addition_record_0),
            H::hash(&addition_record_1),
            "Two addition records with same commitments and same MMR AOCLs must agree."
        );

        let addition_record_1: AdditionRecord = commit::<H>(
            &H::hash(&1451u128),
            &H::hash(&1480u128),
            &H::hash(&1481u128),
        );

        // Verify behavior with empty mutator sets. All empty MS' are the same.
        assert_ne!(
            H::hash(&addition_record_0),
            H::hash(&addition_record_1),
            "Two addition records with differing commitments but same MMR AOCLs must differ."
        );
    }

    #[test]
    fn serialization_test() {
        type H = Tip5;

        let item = H::hash(&1492u128);
        let sender_randomness = H::hash(&1522u128);
        let receiver_digest = H::hash(&1583u128);
        let addition_record: AdditionRecord =
            commit::<H>(&item, &sender_randomness, &receiver_digest);
        let json = serde_json::to_string(&addition_record).unwrap();
        let s_back = serde_json::from_str::<AdditionRecord>(&json).unwrap();
        assert_eq!(
            addition_record.canonical_commitment,
            s_back.canonical_commitment
        );
    }
}
