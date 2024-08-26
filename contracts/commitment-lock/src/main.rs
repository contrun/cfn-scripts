#![no_std]
#![cfg_attr(not(test), no_main)]

#[cfg(test)]
extern crate alloc;

use ckb_hash::blake2b_256;
#[cfg(not(test))]
use ckb_std::default_alloc;
#[cfg(not(test))]
ckb_std::entry!(program_entry);
#[cfg(not(test))]
default_alloc!();

use alloc::{ffi::CString, vec::Vec};
use ckb_std::{
    ckb_constants::Source,
    ckb_types::{bytes::Bytes, core::ScriptHashType, prelude::*},
    error::SysError,
    high_level::{
        exec_cell, load_cell_capacity, load_cell_data, load_cell_lock, load_cell_type,
        load_input_since, load_script, load_tx_hash, load_witness,
    },
    since::Since,
};
use hex::encode;
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/auth_code_hash.rs"));

#[repr(i8)]
pub enum Error {
    IndexOutOfBound = 1,
    ItemMissing,
    LengthNotEnough,
    Encoding,
    // Add customized errors here...
    MultipleInputs,
    InvalidSince,
    InvalidUnlockType,
    InvalidHtlcType,
    ArgsLenError,
    WitnessLenError,
    EmptyWitnessArgsError,
    WitnessHashError,
    OutputCapacityError,
    OutputLockError,
    OutputTypeError,
    OutputUdtAmountError,
    PreimageError,
    AuthError,
}

impl From<SysError> for Error {
    fn from(err: SysError) -> Self {
        match err {
            SysError::IndexOutOfBound => Self::IndexOutOfBound,
            SysError::ItemMissing => Self::ItemMissing,
            SysError::LengthNotEnough(_) => Self::LengthNotEnough,
            SysError::Encoding => Self::Encoding,
            SysError::Unknown(err_code) => panic!("unexpected sys error {}", err_code),
        }
    }
}

pub fn program_entry() -> i8 {
    match auth() {
        Ok(_) => 0,
        Err(err) => err as i8,
    }
}

// a placeholder for empty witness args, to resolve the issue of xudt compatibility
const EMPTY_WITNESS_ARGS: [u8; 16] = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
// min witness script length: 8 (local_delay_epoch) + 20 (local_delay_pubkey_hash) + 20 (revocation_pubkey_hash) = 48
const MIN_WITNESS_SCRIPT_LEN: usize = 48;
// HTLC script length: 1 (htlc_type) + 16 (payment_amount) + 20 (payment_hash) + 20 (remote_htlc_pubkey_hash) + 20 (local_htlc_pubkey_hash) + 8 (htlc_expiry) = 85
const HTLC_SCRIPT_LEN: usize = 85;
// 1 (unlock_type) + 65 (signature) = 66
const UNLOCK_WITH_SIGNATURE_LEN: usize = 66;
const PREIMAGE_LEN: usize = 32;
const MIN_WITNESS_LEN: usize = MIN_WITNESS_SCRIPT_LEN + UNLOCK_WITH_SIGNATURE_LEN;

enum HtlcType {
    Offered,
    Received,
}

enum PaymentHashType {
    Blake2b,
    Sha256,
}

struct Htlc<'a>(&'a [u8]);

impl<'a> Htlc<'a> {
    pub fn htlc_type(&self) -> HtlcType {
        if self.0[0] & 0b00000001 == 0 {
            HtlcType::Offered
        } else {
            HtlcType::Received
        }
    }

    pub fn payment_hash_type(&self) -> PaymentHashType {
        if (self.0[0] >> 1) & 0b0000001 == 0 {
            PaymentHashType::Blake2b
        } else {
            PaymentHashType::Sha256
        }
    }

    pub fn payment_amount(&self) -> u128 {
        u128::from_le_bytes(self.0[1..17].try_into().unwrap())
    }

    pub fn payment_hash(&self) -> &'a [u8] {
        &self.0[17..37]
    }

    pub fn remote_htlc_pubkey_hash(&self) -> &'a [u8] {
        &self.0[37..57]
    }

    pub fn local_htlc_pubkey_hash(&self) -> &'a [u8] {
        &self.0[57..77]
    }

    pub fn htlc_expiry(&self) -> u64 {
        u64::from_le_bytes(self.0[77..85].try_into().unwrap())
    }
}

fn auth() -> Result<(), Error> {
    // since local_delay_pubkey and revocation_pubkey are derived, the scripts are usually unique,
    // to simplify the implementation of the following unlocking logic, we check the number of inputs should be 1
    if load_input_since(1, Source::GroupInput).is_ok() {
        return Err(Error::MultipleInputs);
    }

    // no need to check the type script is sudt / xudt or not, because the offchain tx collaboration will ensure the correct type script.
    let type_script = load_cell_type(0, Source::GroupInput)?;

    let script = load_script()?;
    let args: Bytes = script.args().unpack();
    if args.len() != 20 {
        return Err(Error::ArgsLenError);
    }
    let mut witness = load_witness(0, Source::GroupInput)?;
    if witness
        .drain(0..EMPTY_WITNESS_ARGS.len())
        .collect::<Vec<_>>()
        != EMPTY_WITNESS_ARGS
    {
        return Err(Error::EmptyWitnessArgsError);
    }
    let witness_len = witness.len();
    if witness_len < MIN_WITNESS_LEN {
        return Err(Error::WitnessLenError);
    }
    let (pending_htlcs, preimage) = match witness_len {
        MIN_WITNESS_LEN => (0, None),
        _ => match (witness_len - MIN_WITNESS_LEN) % HTLC_SCRIPT_LEN {
            0 => ((witness_len - MIN_WITNESS_LEN) / HTLC_SCRIPT_LEN, None),
            PREIMAGE_LEN => (
                (witness_len - PREIMAGE_LEN - MIN_WITNESS_LEN) / HTLC_SCRIPT_LEN,
                Some(&witness[witness_len - PREIMAGE_LEN..]),
            ),
            _ => return Err(Error::WitnessLenError),
        },
    };

    // verify the hash of the witness script part is equal to the script args
    let witness_script_len = witness_len
        - if preimage.is_some() {
            UNLOCK_WITH_SIGNATURE_LEN + PREIMAGE_LEN
        } else {
            UNLOCK_WITH_SIGNATURE_LEN
        };
    if blake2b_256(&witness[0..witness_script_len])[0..20] != args[0..20] {
        ckb_std::debug!(
            "witnesses {:?}\nwitnesses hash {:?}\nargs {:?}",
            &witness[0..witness_script_len],
            blake2b_256(&witness[0..witness_script_len]),
            args
        );
        return Err(Error::WitnessHashError);
    }

    let unlock_type = witness[witness_script_len];
    let signature = witness[witness_script_len + 1..witness_script_len + 66].to_vec();

    let message = load_tx_hash()?;
    let mut pubkey_hash = [0u8; 20];

    if unlock_type == 0xFF {
        // unlock with revocation or local_delay pubkey
        let raw_since_value = load_input_since(0, Source::GroupInput)?;
        if raw_since_value == 0 {
            // when input since is 0, it means the unlock logic is for revocation, verify the revocation pubkey
            pubkey_hash.copy_from_slice(&witness[28..48]);
        } else {
            // when input since is not 0, it means the unlock logic is for local_delay, verify the local_delay pubkey and delay
            let since = Since::new(raw_since_value);
            let local_delay_epoch =
                Since::new(u64::from_le_bytes(witness[0..8].try_into().unwrap()));
            if since >= local_delay_epoch {
                pubkey_hash.copy_from_slice(&witness[8..28]);
            } else {
                return Err(Error::InvalidSince);
            }
        }
    } else {
        let unlock_htlc = unlock_type as usize;
        if unlock_htlc >= pending_htlcs {
            return Err(Error::InvalidUnlockType);
        }

        let mut new_amount = if type_script.is_some() {
            let input_cell_data = load_cell_data(0, Source::GroupInput)?;
            u128::from_le_bytes(input_cell_data[0..16].try_into().unwrap())
        } else {
            load_cell_capacity(0, Source::GroupInput)? as u128
        };
        let mut new_witness_script: Vec<&[u8]> = Vec::new();
        new_witness_script.push(&witness[0..MIN_WITNESS_SCRIPT_LEN]);

        for (i, htlc_script) in witness[MIN_WITNESS_SCRIPT_LEN..witness_script_len]
            .chunks(HTLC_SCRIPT_LEN)
            .enumerate()
        {
            let htlc = Htlc(htlc_script);
            if unlock_htlc == i {
                match htlc.htlc_type() {
                    HtlcType::Offered => {
                        let raw_since_value = load_input_since(0, Source::GroupInput)?;
                        if raw_since_value == 0 {
                            // when input since is 0, it means the unlock logic is for remote_htlc pubkey and preimage
                            if preimage
                                .map(|p| match htlc.payment_hash_type() {
                                    PaymentHashType::Blake2b => {
                                        htlc.payment_hash() != &blake2b_256(p)[0..20]
                                    }
                                    PaymentHashType::Sha256 => {
                                        htlc.payment_hash() != &Sha256::digest(p)[0..20]
                                    }
                                })
                                .unwrap_or(true)
                            {
                                return Err(Error::PreimageError);
                            }
                            new_amount -= htlc.payment_amount();
                            pubkey_hash.copy_from_slice(htlc.remote_htlc_pubkey_hash());
                        } else {
                            // when input since is not 0, it means the unlock logic is for local_htlc pubkey and htlc expiry
                            let since = Since::new(raw_since_value);
                            let htlc_expiry = Since::new(htlc.htlc_expiry());
                            if since >= htlc_expiry {
                                pubkey_hash.copy_from_slice(htlc.local_htlc_pubkey_hash());
                            } else {
                                return Err(Error::InvalidSince);
                            }
                        }
                    }
                    HtlcType::Received => {
                        let raw_since_value = load_input_since(0, Source::GroupInput)?;
                        if raw_since_value == 0 {
                            // when input since is 0, it means the unlock logic is for local_htlc pubkey and preimage
                            if preimage
                                .map(|p| match htlc.payment_hash_type() {
                                    PaymentHashType::Blake2b => {
                                        htlc.payment_hash() != &blake2b_256(p)[0..20]
                                    }
                                    PaymentHashType::Sha256 => {
                                        htlc.payment_hash() != &Sha256::digest(p)[0..20]
                                    }
                                })
                                .unwrap_or(true)
                            {
                                return Err(Error::PreimageError);
                            }
                            pubkey_hash.copy_from_slice(htlc.local_htlc_pubkey_hash());
                        } else {
                            // when input since is not 0, it means the unlock logic is for remote_htlc pubkey and htlc expiry
                            let since = Since::new(raw_since_value);
                            let htlc_expiry = Since::new(htlc.htlc_expiry());
                            if since >= htlc_expiry {
                                new_amount -= htlc.payment_amount();
                                pubkey_hash.copy_from_slice(htlc.remote_htlc_pubkey_hash());
                            } else {
                                return Err(Error::InvalidSince);
                            }
                        }
                    }
                }
            } else {
                new_witness_script.push(htlc_script);
            }
        }

        // verify the first output cell's lock script is correct
        let output_lock = load_cell_lock(0, Source::Output)?;
        let expected_lock_args = blake2b_256(new_witness_script.concat())[0..20].pack();
        if output_lock.code_hash() != script.code_hash()
            || output_lock.hash_type() != script.hash_type()
            || output_lock.args() != expected_lock_args
        {
            return Err(Error::OutputLockError);
        }

        match type_script {
            Some(udt_script) => {
                // verify the first output cell's capacity, type script and udt amount are correct
                let output_capacity = load_cell_capacity(0, Source::Output)?;
                let input_capacity = load_cell_capacity(0, Source::GroupInput)?;
                if output_capacity != input_capacity {
                    return Err(Error::OutputCapacityError);
                }

                let output_type = load_cell_type(0, Source::Output)?;
                if output_type != Some(udt_script) {
                    return Err(Error::OutputTypeError);
                }

                let output_data = load_cell_data(0, Source::Output)?;
                let output_amount = u128::from_le_bytes(output_data[0..16].try_into().unwrap());
                if output_amount != new_amount {
                    return Err(Error::OutputUdtAmountError);
                }
            }
            None => {
                // verify the first output cell's capacity is correct
                let output_capacity = load_cell_capacity(0, Source::Output)? as u128;
                if output_capacity != new_amount {
                    return Err(Error::OutputCapacityError);
                }
            }
        }
    }

    // AuthAlgorithmIdCkb = 0
    let algorithm_id_str = CString::new(encode([0u8])).unwrap();
    let signature_str = CString::new(encode(signature)).unwrap();
    let message_str = CString::new(encode(message)).unwrap();
    let pubkey_hash_str = CString::new(encode(pubkey_hash)).unwrap();
    ckb_std::debug!("algorithm_id: {:?}", algorithm_id_str);
    ckb_std::debug!("signature: {:?}", signature_str);
    ckb_std::debug!("message: {:?}", message_str);
    ckb_std::debug!("pubkey_hash: {:?}", pubkey_hash_str);

    let args = [
        algorithm_id_str.as_c_str(),
        signature_str.as_c_str(),
        message_str.as_c_str(),
        pubkey_hash_str.as_c_str(),
    ];

    if let Err(err) = exec_cell(&AUTH_CODE_HASH, ScriptHashType::Data1, &args) {
        ckb_std::debug!("exec ckb-auth error: {:?}", err);
        return Err(Error::AuthError);
    }
    Ok(())
}
