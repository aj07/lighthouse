use std::collections::HashSet;
use std::sync::Arc;
use super::structs::AttestationRecord;
use super::AttesterMap;
use super::attestation_parent_hashes::{
    attestation_parent_hashes,
    ParentHashesError,
};
use super::db::{
    ClientDB,
    DBError
};
use super::db::stores::{
    BlockStore,
    ValidatorStore,
};
use super::utils::types::{
    Hash256,
};
use super::message_generation::generate_signed_message;
use super::signature_verification::{
    verify_aggregate_signature_for_indices,
    SignatureVerificationError,
};

#[derive(Debug,PartialEq)]
pub enum AttestationValidationError {
    SlotTooHigh,
    SlotTooLow,
    JustifiedSlotTooHigh,
    UnknownJustifiedBlock,
    TooManyObliqueHashes,
    BadCurrentHashes,
    BadObliqueHashes,
    BadAttesterMap,
    IntWrapping,
    PublicKeyCorrupt,
    NoPublicKeyForValidator,
    BadBitfieldLength,
    InvalidBitfield,
    InvalidBitfieldEndBits,
    NoSignatures,
    NonZeroTrailingBits,
    AggregateSignatureFail,
    DBError(String),
}

fn bytes_for_bits(bits: usize) -> usize {
    (bits.saturating_sub(1) / 8) + 1
}

fn any_of_last_n_bits_are_set(byte: u8, n: usize) -> bool {
    let shift = 8_u16.saturating_sub(n as u16);
    ((!0 << shift) & u16::from(byte)) > 0
}

pub fn validate_attestation<T>(a: &AttestationRecord,
                               block_slot: u64,
                               cycle_length: u8,
                               known_last_justified_slot: u64,
                               known_parent_hashes: &Arc<Vec<Hash256>>,
                               block_store: &Arc<BlockStore<T>>,
                               validator_store: &Arc<ValidatorStore<T>>,
                               attester_map: &Arc<AttesterMap>)
    -> Result<Option<HashSet<usize>>, AttestationValidationError>
    where T: ClientDB + Sized
{
    /*
     * The attesation slot must not be higher than the block that contained it.
     */
    if a.slot > block_slot {
        return Err(AttestationValidationError::SlotTooHigh);
    }

    /*
     * The slot of this attestation must not be more than cycle_length + 1 distance
     * from the block that contained it.
     *
     * The below code stays overflow-safe as long as cycle length is a < 64 bit integer.
     */
    if a.slot < block_slot.saturating_sub(u64::from(cycle_length) + 1) {
        return Err(AttestationValidationError::SlotTooLow);
    }

    /*
     * The attestation must indicate that its last justified slot is the same as the last justified
     * slot known to us.
     */
    if a.justified_slot > known_last_justified_slot {
        return Err(AttestationValidationError::JustifiedSlotTooHigh);
    }

    /*
     * There is no need to include more oblique parents hashes than there are blocks
     * in a cycle.
     */
    if a.oblique_parent_hashes.len() > usize::from(cycle_length) {
        return Err(AttestationValidationError::TooManyObliqueHashes);
    }

    /*
     * Retrieve the set of attestation indices for this slot and shard id.
     *
     * This is an array mapping the order that validators will appear in the bitfield to the
     * canonincal index of a validator.
     */
    let attestation_indices = attester_map.get(&(a.slot, a.shard_id))
        .ok_or(AttestationValidationError::BadAttesterMap)?;

    /*
     * The bitfield must be no longer than the minimum required to represent each validator in the
     * attestation indicies for this slot and shard id.
     */
    if a.attester_bitfield.num_bytes() !=
        bytes_for_bits(attestation_indices.len())
    {
        return Err(AttestationValidationError::BadBitfieldLength);
    }

    /*
     * If there are excess bits in the bitfield because the number of a validators in not a
     * multiple of 8, reject this attestation record.
     *
     * Allow extra set bits would permit mutliple different byte layouts (and therefore hashes) to
     * refer to the same AttesationRecord.
     */
    let last_byte =
        a.attester_bitfield.get_byte(a.attester_bitfield.num_bytes() - 1)
        .ok_or(AttestationValidationError::InvalidBitfield)?;
    if any_of_last_n_bits_are_set(*last_byte, a.attester_bitfield.len() % 8) {
        return Err(AttestationValidationError::InvalidBitfieldEndBits)
    }

    /*
     * The specified justified block hash must be known to us
     */
    if !block_store.block_exists(&a.justified_block_hash)? {
        return Err(AttestationValidationError::UnknownJustifiedBlock)
    }

    let signed_message = {
        let parent_hashes = attestation_parent_hashes(
            cycle_length,
            block_slot,
            a.slot,
            &known_parent_hashes,
            &a.oblique_parent_hashes)?;
        generate_signed_message(
            a.slot,
            &parent_hashes,
            a.shard_id,
            &a.shard_block_hash,
            a.justified_slot)
    };

    let voted_hashmap =
        verify_aggregate_signature_for_indices(
            &signed_message,
            &a.aggregate_sig,
            &attestation_indices,
            &a.attester_bitfield,
            &validator_store)?;

    Ok(voted_hashmap)
}

impl From<ParentHashesError> for AttestationValidationError {
    fn from(e: ParentHashesError) -> Self {
        match e {
            ParentHashesError::BadCurrentHashes
                => AttestationValidationError::BadCurrentHashes,
            ParentHashesError::BadObliqueHashes
                => AttestationValidationError::BadObliqueHashes,
            ParentHashesError::SlotTooLow
                => AttestationValidationError::SlotTooLow,
            ParentHashesError::SlotTooHigh
                => AttestationValidationError::SlotTooHigh,
            ParentHashesError::IntWrapping
                => AttestationValidationError::IntWrapping
        }
    }
}

impl From<DBError> for AttestationValidationError {
    fn from(e: DBError) -> Self {
        AttestationValidationError::DBError(e.message)
    }
}

impl From<SignatureVerificationError> for AttestationValidationError {
    fn from(e: SignatureVerificationError) -> Self {
        match e {
            SignatureVerificationError::BadValidatorIndex
                => AttestationValidationError::BadAttesterMap,
            SignatureVerificationError::PublicKeyCorrupt
                => AttestationValidationError::PublicKeyCorrupt,
            SignatureVerificationError::NoPublicKeyForValidator
                => AttestationValidationError::NoPublicKeyForValidator,
            SignatureVerificationError::DBError(s)
                => AttestationValidationError::DBError(s),
        }
    }
}