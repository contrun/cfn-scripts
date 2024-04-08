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

use alloc::ffi::CString;
use alloc::format;
use ckb_std::{
    ckb_constants::Source,
    ckb_types::{bytes::Bytes, core::ScriptHashType, prelude::*},
    error::SysError,
    high_level::{exec_cell, load_input_since, load_script, load_tx_hash, load_witness},
    since::Since,
};
use hex::encode;

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
    ArgsLenError,
    WitnessLenError,
    WitnessHashError,
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

fn auth() -> Result<(), Error> {
    // since local_delayed_pubkey and revocation_pubkey are derived, the scripts are usually unique,
    // to simplify the implementation of the following unlocking logic, we check the number of inputs should be 1
    if load_input_since(1, Source::GroupInput).is_ok() {
        return Err(Error::MultipleInputs);
    }

    let script = load_script()?;
    let args: Bytes = script.args().unpack();
    if args.len() != 20 {
        return Err(Error::ArgsLenError);
    }
    let witness = load_witness(0, Source::GroupInput)?;
    if witness.len() != 8 + 20 + 20 + 65 {
        return Err(Error::WitnessLenError);
    }
    if blake2b_256(&witness[0..48])[0..20] != args[0..20] {
        return Err(Error::WitnessHashError);
    }

    let message = load_tx_hash()?;
    let mut pubkey_hash = [0u8; 20];
    let raw_since_value = load_input_since(0, Source::GroupInput)?;
    if raw_since_value == 0 {
        pubkey_hash.copy_from_slice(&witness[28..48]);
    } else {
        let since = Since::new(raw_since_value);
        let to_self_delay = Since::new(u64::from_le_bytes(witness[0..8].try_into().unwrap()));
        if since >= to_self_delay {
            pubkey_hash.copy_from_slice(&witness[8..28]);
        } else {
            return Err(Error::InvalidSince);
        }
    }

    // AuthAlgorithmIdCkb = 0
    let algorithm_id_str = CString::new(format!("{:02X?}", 0u8)).unwrap();
    let signature_str = CString::new(encode(&witness[48..])).unwrap();
    let message_str = CString::new(encode(message)).unwrap();
    let pubkey_hash_str = CString::new(encode(pubkey_hash)).unwrap();

    let args = [
        algorithm_id_str.as_c_str(),
        signature_str.as_c_str(),
        message_str.as_c_str(),
        pubkey_hash_str.as_c_str(),
    ];

    exec_cell(&AUTH_CODE_HASH, ScriptHashType::Data1, &args).map_err(|_| Error::AuthError)?;
    Ok(())
}
