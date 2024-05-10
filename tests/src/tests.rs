use super::*;
use ckb_std::since::{EpochNumberWithFraction, Since};
use ckb_testtool::{
    ckb_crypto::{
        self,
        secp::{Generator, Pubkey},
    },
    ckb_hash::blake2b_256,
    ckb_types::{bytes::Bytes, core::TransactionBuilder, packed::*, prelude::*},
    context::Context,
};
use musig2::{
    BinaryEncoding, CompactSignature, FirstRound, KeyAggContext, PartialSignature, SecNonceSpices,
};
use secp256k1::{
    rand::{self, RngCore},
    PublicKey, Secp256k1, SecretKey,
};

const MAX_CYCLES: u64 = 10_000_000;

const BYTE_SHANNONS: u64 = 100_000_000;

#[test]
fn test_funding_lock() {
    // deploy contract
    let mut context = Context::default();
    let loader = Loader::default();
    let funding_lock_bin = loader.load_binary("funding-lock");
    let auth_bin = loader.load_binary("../../deps/auth");
    let funding_lock_out_point = context.deploy_cell(funding_lock_bin);
    let auth_out_point = context.deploy_cell(auth_bin);

    // generate two random secret keys
    let sec_key_1 = SecretKey::new(&mut rand::thread_rng());
    let sec_key_2 = SecretKey::new(&mut rand::thread_rng());

    // public key aggregation
    let secp256k1 = Secp256k1::new();
    let pub_key_1 = sec_key_1.public_key(&secp256k1);
    let pub_key_2 = sec_key_2.public_key(&secp256k1);
    let key_agg_ctx = KeyAggContext::new(vec![pub_key_1, pub_key_2]).unwrap();
    let aggregated_pub_key: PublicKey = key_agg_ctx.aggregated_pubkey();
    let x_only_pub_key = aggregated_pub_key.x_only_public_key().0.serialize();

    // prepare scripts
    let pub_key_hash = blake2b_256(x_only_pub_key);
    let lock_script = context
        .build_script(&funding_lock_out_point, pub_key_hash[0..20].to_vec().into())
        .expect("script");

    // prepare cell deps
    let funding_lock_dep = CellDep::new_builder()
        .out_point(funding_lock_out_point)
        .build();
    let auth_dep = CellDep::new_builder().out_point(auth_out_point).build();
    let cell_deps = vec![funding_lock_dep, auth_dep].pack();

    // prepare cells
    let input_out_point = context.create_cell(
        CellOutput::new_builder()
            .capacity(1000u64.pack())
            .lock(lock_script.clone())
            .build(),
        Bytes::new(),
    );
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .build();
    let output_lock = Script::new_builder()
        .args(Bytes::from("output_lock").pack())
        .build();
    let outputs = vec![
        CellOutput::new_builder()
            .capacity(500u64.pack())
            .lock(output_lock.clone())
            .build(),
        CellOutput::new_builder()
            .capacity(500u64.pack())
            .lock(output_lock)
            .build(),
    ];

    let outputs_data = vec![Bytes::new(); 2];

    // build transaction
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign and add witness
    let tx_hash: [u8; 32] = tx.hash().as_slice().try_into().unwrap();
    let version = 0u64.to_le_bytes();
    let funding_out_point = input_out_point.as_slice();
    let message = blake2b_256(
        [
            version.to_vec(),
            funding_out_point.to_vec(),
            tx_hash.to_vec(),
        ]
        .concat(),
    );

    let mut first_round_1 = {
        let mut nonce_seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce_seed);

        FirstRound::new(
            key_agg_ctx.clone(),
            nonce_seed,
            0,
            SecNonceSpices::new()
                .with_seckey(sec_key_1)
                .with_message(&message),
        )
        .unwrap()
    };

    let mut first_round_2 = {
        let mut nonce_seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce_seed);

        FirstRound::new(
            key_agg_ctx,
            nonce_seed,
            1,
            SecNonceSpices::new()
                .with_seckey(sec_key_2)
                .with_message(&message),
        )
        .unwrap()
    };

    first_round_1
        .receive_nonce(1, first_round_2.our_public_nonce())
        .unwrap();
    first_round_2
        .receive_nonce(0, first_round_1.our_public_nonce())
        .unwrap();

    let mut second_round_1 = first_round_1.finalize(sec_key_1, &message).unwrap();
    let mut second_round_2 = first_round_2.finalize(sec_key_2, &message).unwrap();
    let signature_1: PartialSignature = second_round_1.our_signature();
    let signature_2: PartialSignature = second_round_2.our_signature();

    second_round_1.receive_signature(1, signature_2).unwrap();
    let aggregated_signature_1: CompactSignature = second_round_1.finalize().unwrap();
    second_round_2.receive_signature(0, signature_1).unwrap();
    let aggregated_signature_2: CompactSignature = second_round_2.finalize().unwrap();

    assert_eq!(aggregated_signature_1, aggregated_signature_2);
    println!("signature: {:?}", aggregated_signature_1.to_bytes());

    let witness = [
        version.to_vec(),
        funding_out_point.to_vec(),
        x_only_pub_key.to_vec(),
        aggregated_signature_1.to_bytes().to_vec(),
    ]
    .concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();

    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);
}

#[test]
fn test_commitment_lock_no_pending_htlcs() {
    // deploy contract
    let mut context = Context::default();
    let loader = Loader::default();
    let commitment_lock_bin = loader.load_binary("commitment-lock");
    let auth_bin = loader.load_binary("../../deps/auth");
    let commitment_lock_out_point = context.deploy_cell(commitment_lock_bin);
    let auth_out_point = context.deploy_cell(auth_bin);

    // prepare script
    let mut generator = Generator::new();
    // 42 hours = 4.5 epochs
    let local_delay_epoch = Since::from_epoch(EpochNumberWithFraction::new(10, 1, 2), false);
    let local_delay_epoch_key = generator.gen_keypair();
    let revocation_key = generator.gen_keypair();

    let witness_script = [
        local_delay_epoch.as_u64().to_le_bytes().to_vec(),
        blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
        blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
    ]
    .concat();

    let args = blake2b_256(&witness_script)[0..20].to_vec();

    let lock_script = context
        .build_script(&commitment_lock_out_point, args.into())
        .expect("script");

    // prepare cell deps
    let commitment_lock_dep = CellDep::new_builder()
        .out_point(commitment_lock_out_point)
        .build();
    let auth_dep = CellDep::new_builder().out_point(auth_out_point).build();
    let cell_deps = vec![commitment_lock_dep, auth_dep].pack();

    // prepare cells
    let input_out_point = context.create_cell(
        CellOutput::new_builder()
            .capacity(1000u64.pack())
            .lock(lock_script.clone())
            .build(),
        Bytes::new(),
    );
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .build();
    let output_lock = Script::new_builder()
        .args(Bytes::from("output_lock").pack())
        .build();
    let outputs = vec![
        CellOutput::new_builder()
            .capacity(500u64.pack())
            .lock(output_lock.clone())
            .build(),
        CellOutput::new_builder()
            .capacity(500u64.pack())
            .lock(output_lock)
            .build(),
    ];

    let outputs_data = vec![Bytes::new(); 2];

    // build transaction with revocation unlock logic
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs.clone())
        .outputs_data(outputs_data.pack())
        .build();

    // sign with revocation key
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = revocation_key
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [witness_script.clone(), vec![0xFF], signature].concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // build transaction with local_delay_epoch unlock logic
    // delay 48 hours
    let since = Since::from_epoch(EpochNumberWithFraction::new(12, 0, 1), false);
    let input = CellInput::new_builder()
        .previous_output(input_out_point)
        .since(since.as_u64().pack())
        .build();

    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with local_delay_epoch_key
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = local_delay_epoch_key
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [witness_script, vec![0xFF], signature].concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);
}

type ShortHash = [u8; 20];
type Hash = [u8; 32];

/// A tlc output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TLC {
    /// The id of a received TLC. Must be empty if this is an offered HTLC.
    /// We will fill in the id when we send this tlc to the counterparty.
    /// Otherwise must be the next sequence number of the counterparty.
    pub id: u64,
    /// Is this HTLC being received by us or offered by us?
    pub is_offered: bool,
    /// The value as it appears in the commitment transaction
    pub amount: u128,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: Since,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Hash,
    /// The preimage of the hash to be sent to the counterparty.
    pub payment_preimage: Option<Hash>,
    pub local_pubkey: ckb_crypto::secp::Pubkey,
    pub remote_pubkey: ckb_crypto::secp::Pubkey,
}

impl TLC {
    fn new(
        id: u64,
        is_offered: bool,
        amount: u128,
        lock_time: Since,
        payment_hash: Hash,
        payment_preimage: Option<Hash>,
        local_pubkey: ckb_crypto::secp::Pubkey,
        remote_pubkey: ckb_crypto::secp::Pubkey,
    ) -> Self {
        Self {
            id,
            is_offered,
            amount,
            lock_time,
            payment_hash,
            payment_preimage,
            local_pubkey,
            remote_pubkey,
        }
    }

    fn get_hash(&self) -> Hash {
        self.payment_preimage
            .map(|x| blake2b_256(x))
            .unwrap_or(self.payment_hash)
    }

    fn get_short_hash(&self) -> ShortHash {
        self.get_hash()[0..20].try_into().unwrap()
    }

    fn get_local_pubkey_hash(&self) -> ShortHash {
        blake2b_256(self.local_pubkey.serialize())[0..20]
            .try_into()
            .unwrap()
    }

    fn get_remote_pubkey_hash(&self) -> ShortHash {
        blake2b_256(self.remote_pubkey.serialize())[0..20]
            .try_into()
            .unwrap()
    }
}

struct AugmentedTransaction {
    tx: super::TransactionView,
    lock_script: Script,
    witness_script: Vec<u8>,
}

struct CommitmentLockContext {
    context: Context,
    lock_script_outpoint: OutPoint,
    cell_deps: CellDepVec,
}

impl CommitmentLockContext {
    fn new() -> Self {
        // deploy contract
        let mut context = Context::default();
        let loader = Loader::default();
        let commitment_lock_bin = loader.load_binary("commitment-lock");
        let auth_bin = loader.load_binary("../../deps/auth");
        let commitment_lock_out_point = context.deploy_cell(commitment_lock_bin);
        let auth_out_point = context.deploy_cell(auth_bin);

        // prepare cell deps
        let commitment_lock_dep = CellDep::new_builder()
            .out_point(commitment_lock_out_point.clone())
            .build();
        dbg!(&commitment_lock_out_point);
        let auth_dep = CellDep::new_builder().out_point(auth_out_point).build();
        let cell_deps = vec![commitment_lock_dep, auth_dep].pack();
        Self {
            context,
            lock_script_outpoint: commitment_lock_out_point,
            cell_deps,
        }
    }

    fn get_witnesses(
        &self,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
    ) -> Vec<u8> {
        // TODO: This was copied from the test below. It should be refactored to be more generic.
        assert_eq!(tlcs.len(), 2);
        let TLC {
            amount: payment_amount1,
            payment_hash: preimage1,
            remote_pubkey: remote_htlc_key1,
            local_pubkey: local_htlc_key1,
            lock_time: expiry1,
            is_offered: is_offered1,
            ..
        } = &tlcs[0];
        let TLC {
            amount: payment_amount2,
            payment_hash: preimage2,
            remote_pubkey: remote_htlc_key2,
            local_pubkey: local_htlc_key2,
            lock_time: expiry2,
            is_offered: is_offered2,
            ..
        } = &tlcs[1];
        let witness_script = [
            local_delay_epoch.as_u64().to_le_bytes().to_vec(),
            blake2b_256(local_delay_epoch_key.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.serialize())[0..20].to_vec(),
            (if *is_offered1 { [0] } else { [1] }).to_vec(),
            payment_amount1.to_le_bytes().to_vec(),
            blake2b_256(preimage1)[0..20].to_vec(),
            blake2b_256(remote_htlc_key1.serialize())[0..20].to_vec(),
            blake2b_256(local_htlc_key1.serialize())[0..20].to_vec(),
            expiry1.as_u64().to_le_bytes().to_vec(),
            (if *is_offered2 { [0] } else { [1] }).to_vec(),
            payment_amount2.to_le_bytes().to_vec(),
            blake2b_256(preimage2)[0..20].to_vec(),
            blake2b_256(remote_htlc_key2.serialize())[0..20].to_vec(),
            blake2b_256(local_htlc_key2.serialize())[0..20].to_vec(),
            expiry2.as_u64().to_le_bytes().to_vec(),
        ]
        .concat();
        witness_script
    }

    fn create_commitment_cell_with_aux_data(
        &mut self,
        capacity: u64,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
    ) -> (OutPoint, Vec<u8>, Script) {
        let witness_script = self.get_witnesses(
            local_delay_epoch,
            local_delay_epoch_key,
            revocation_key,
            tlcs,
        );

        let args = blake2b_256(&witness_script)[0..20].to_vec();

        let lock_script = self
            .context
            .build_script(&self.lock_script_outpoint, args.into())
            .expect("script");

        // prepare cells
        (
            self.context.create_cell(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(lock_script.clone())
                    .build(),
                Bytes::new(),
            ),
            witness_script,
            lock_script,
        )
    }

    fn create_augmented_tx(
        &mut self,
        capacity: u64,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
        outputs: Vec<CellOutput>,
        outputs_data: Vec<Bytes>,
    ) -> AugmentedTransaction {
        let (input_out_point, witness_script, lock_script) = self
            .create_commitment_cell_with_aux_data(
                capacity,
                local_delay_epoch,
                local_delay_epoch_key,
                revocation_key,
                tlcs,
            );

        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .build();

        // build transaction with revocation unlock logic
        let tx = TransactionBuilder::default()
            .cell_deps(self.cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        AugmentedTransaction {
            tx,
            lock_script,
            witness_script,
        }
    }
}

#[test]
fn test_commitment_lock_with_two_pending_htlcs() {
    // prepare script
    let mut generator = Generator::new();
    // 42 hours = 4.5 epochs
    let local_delay_epoch = Since::from_epoch(EpochNumberWithFraction::new(10, 1, 2), false);
    let local_delay_epoch_key = generator.gen_keypair();
    let revocation_key = generator.gen_keypair();
    let remote_htlc_key1 = generator.gen_keypair();
    let remote_htlc_key2 = generator.gen_keypair();
    let local_htlc_key1 = generator.gen_keypair();
    let local_htlc_key2 = generator.gen_keypair();
    let preimage1 = [42u8; 32];
    let preimage2 = [24u8; 32];
    let payment_amount1 = 5 * BYTE_SHANNONS as u128;
    let payment_amount2 = 8 * BYTE_SHANNONS as u128;
    // timeout after 2024-04-01 01:00:00
    let expiry1 = Since::from_timestamp(1711976400, true).unwrap();
    // timeout after 2024-04-02 01:00:00
    let expiry2 = Since::from_timestamp(1712062800, true).unwrap();

    let tlcs = vec![
        TLC::new(
            0,
            true,
            payment_amount1,
            expiry1,
            preimage1,
            None,
            local_htlc_key1.1.clone(),
            remote_htlc_key1.1.clone(),
        ),
        TLC::new(
            1,
            false,
            payment_amount2,
            expiry2,
            preimage2,
            None,
            local_htlc_key2.1.clone(),
            remote_htlc_key2.1.clone(),
        ),
    ];

    let mut context = CommitmentLockContext::new();

    let output_lock = Script::new_builder()
        .args(Bytes::from("output_lock").pack())
        .build();

    let outputs = vec![
        CellOutput::new_builder()
            .capacity((500 * BYTE_SHANNONS).pack())
            .lock(output_lock.clone())
            .build(),
        CellOutput::new_builder()
            .capacity((500 * BYTE_SHANNONS).pack())
            .lock(output_lock.clone())
            .build(),
    ];
    let outputs_data = vec![Bytes::new(); 2];

    let AugmentedTransaction {
        tx,
        lock_script,
        witness_script,
    } = context.create_augmented_tx(
        500 * BYTE_SHANNONS,
        local_delay_epoch,
        local_delay_epoch_key.1.clone(),
        revocation_key.1.clone(),
        tlcs,
        outputs.clone(),
        outputs_data.clone(),
    );
    let CommitmentLockContext {
        mut context,
        cell_deps,
        lock_script_outpoint: _,
    } = context;

    let input_out_point = context.create_cell(
        CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS).pack())
            .lock(lock_script.clone())
            .build(),
        Bytes::new(),
    );

    {
        // sign with revocation key
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = revocation_key
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [witness_script.clone(), vec![0xFF], signature].concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);
    }

    {
        // build transaction with local_delay_epoch unlock logic
        // delay 48 hours
        let since = Since::from_epoch(EpochNumberWithFraction::new(12, 0, 1), false);
        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .since(since.as_u64().pack())
            .build();

        let tx = TransactionBuilder::default()
            .cell_deps(cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        // sign with local_delay_epoch_key
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = local_delay_epoch_key
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [witness_script.clone(), vec![0xFF], signature].concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);
    }

    {
        // build transaction with remote_htlc_pubkey unlock offered pending htlc 1
        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .build();

        let new_witness_script = [
            local_delay_epoch.as_u64().to_le_bytes().to_vec(),
            blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
            [1u8].to_vec(),
            payment_amount2.to_le_bytes().to_vec(),
            blake2b_256(preimage2)[0..20].to_vec(),
            blake2b_256(remote_htlc_key2.1.serialize())[0..20].to_vec(),
            blake2b_256(local_htlc_key2.1.serialize())[0..20].to_vec(),
            expiry2.as_u64().to_le_bytes().to_vec(),
        ]
        .concat();
        let new_lock_script = lock_script
            .clone()
            .as_builder()
            .args(blake2b_256(&new_witness_script)[0..20].to_vec().pack())
            .build();
        let outputs = vec![CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS - payment_amount1 as u64).pack())
            .lock(new_lock_script.clone())
            .build()];
        let outputs_data = vec![Bytes::new()];
        let tx = TransactionBuilder::default()
            .cell_deps(cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        // sign with remote_htlc_pubkey
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = remote_htlc_key1
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [
            witness_script.clone(),
            vec![0x00],
            signature,
            preimage1.to_vec(),
        ]
        .concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);

        // build transaction with local_htlc_pubkey unlock offered pending htlc 1
        let since = Since::from_timestamp(1711976400 + 1000, true).unwrap();

        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .since(since.as_u64().pack())
            .build();
        let new_lock_script = lock_script
            .clone()
            .as_builder()
            .args(blake2b_256(&new_witness_script)[0..20].to_vec().pack())
            .build();

        let outputs = vec![CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS).pack())
            .lock(new_lock_script.clone())
            .build()];
        let outputs_data = vec![Bytes::new()];
        let tx = TransactionBuilder::default()
            .cell_deps(cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        // sign with local_htlc_pubkey
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = local_htlc_key1
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [
            witness_script.clone(),
            vec![0x00],
            signature,
            preimage1.to_vec(),
        ]
        .concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);
    }

    {
        // build transaction with remote_htlc_pubkey unlock received pending htlc 2
        let since = Since::from_timestamp(1712062800 + 1000, true).unwrap();
        let input = CellInput::new_builder()
            .since(since.as_u64().pack())
            .previous_output(input_out_point.clone())
            .build();

        let new_witness_script = [
            local_delay_epoch.as_u64().to_le_bytes().to_vec(),
            blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
            [0u8].to_vec(),
            payment_amount1.to_le_bytes().to_vec(),
            blake2b_256(preimage1)[0..20].to_vec(),
            blake2b_256(remote_htlc_key1.1.serialize())[0..20].to_vec(),
            blake2b_256(local_htlc_key1.1.serialize())[0..20].to_vec(),
            expiry1.as_u64().to_le_bytes().to_vec(),
        ]
        .concat();
        let new_lock_script = lock_script
            .as_builder()
            .args(blake2b_256(&new_witness_script)[0..20].to_vec().pack())
            .build();
        let outputs = vec![CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS - payment_amount2 as u64).pack())
            .lock(new_lock_script.clone())
            .build()];
        let outputs_data = vec![Bytes::new()];
        let tx = TransactionBuilder::default()
            .cell_deps(cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        // sign with remote_htlc_pubkey
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = remote_htlc_key2
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [witness_script.clone(), vec![0x01], signature].concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);

        // // build transaction with local_htlc_pubkey unlock received pending htlc 2
        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .build();
        let outputs = vec![CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS).pack())
            .lock(new_lock_script.clone())
            .build()];
        let outputs_data = vec![Bytes::new()];
        let tx = TransactionBuilder::default()
            .cell_deps(cell_deps)
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        // sign with local_htlc_pubkey
        let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

        let signature = local_htlc_key2
            .0
            .sign_recoverable(&message.into())
            .unwrap()
            .serialize();
        let witness = [
            witness_script.clone(),
            vec![0x01],
            signature,
            preimage2.to_vec(),
        ]
        .concat();

        let tx = tx.as_advanced_builder().witness(witness.pack()).build();
        println!("tx: {:?}", tx);

        // run
        let cycles = context
            .verify_tx(&tx, MAX_CYCLES)
            .expect("pass verification");
        println!("consume cycles: {}", cycles);
    }
}

#[test]
fn test_commitment_lock_with_two_pending_htlcs_and_sudt() {
    // deploy contract
    let mut context = Context::default();
    let loader = Loader::default();
    let commitment_lock_bin = loader.load_binary("commitment-lock");
    let auth_bin = loader.load_binary("../../deps/auth");
    let simple_udt_bin = loader.load_binary("../../deps/simple_udt");
    let commitment_lock_out_point = context.deploy_cell(commitment_lock_bin);
    let auth_out_point = context.deploy_cell(auth_bin);
    let simple_udt_out_point = context.deploy_cell(simple_udt_bin);

    // prepare script
    let mut generator = Generator::new();
    // 42 hours = 4.5 epochs
    let local_delay_epoch = Since::from_epoch(EpochNumberWithFraction::new(10, 1, 2), false);
    let local_delay_epoch_key = generator.gen_keypair();
    let revocation_key = generator.gen_keypair();
    let remote_htlc_key1 = generator.gen_keypair();
    let remote_htlc_key2 = generator.gen_keypair();
    let local_htlc_key1 = generator.gen_keypair();
    let local_htlc_key2 = generator.gen_keypair();
    let preimage1 = [42u8; 32];
    let preimage2 = [24u8; 32];
    let payment_amount1 = 1234567890u128;
    let payment_amount2 = 9876543210u128;
    // timeout after 2024-04-01 01:00:00
    let expiry1 = Since::from_timestamp(1711976400, true).unwrap();
    // timeout after 2024-04-02 01:00:00
    let expiry2 = Since::from_timestamp(1712062800, true).unwrap();

    let witness_script = [
        local_delay_epoch.as_u64().to_le_bytes().to_vec(),
        blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
        blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
        [0u8].to_vec(),
        payment_amount1.to_le_bytes().to_vec(),
        blake2b_256(preimage1)[0..20].to_vec(),
        blake2b_256(remote_htlc_key1.1.serialize())[0..20].to_vec(),
        blake2b_256(local_htlc_key1.1.serialize())[0..20].to_vec(),
        expiry1.as_u64().to_le_bytes().to_vec(),
        [1u8].to_vec(),
        payment_amount2.to_le_bytes().to_vec(),
        blake2b_256(preimage2)[0..20].to_vec(),
        blake2b_256(remote_htlc_key2.1.serialize())[0..20].to_vec(),
        blake2b_256(local_htlc_key2.1.serialize())[0..20].to_vec(),
        expiry2.as_u64().to_le_bytes().to_vec(),
    ]
    .concat();

    let args = blake2b_256(&witness_script)[0..20].to_vec();

    let lock_script = context
        .build_script(&commitment_lock_out_point, args.into())
        .expect("script");

    let type_script = context
        .build_script(&simple_udt_out_point, vec![42; 32].into())
        .expect("script");

    // prepare cell deps
    let commitment_lock_dep = CellDep::new_builder()
        .out_point(commitment_lock_out_point)
        .build();
    let auth_dep = CellDep::new_builder().out_point(auth_out_point).build();
    let simple_udt_dep = CellDep::new_builder()
        .out_point(simple_udt_out_point)
        .build();
    let cell_deps = vec![commitment_lock_dep, auth_dep, simple_udt_dep].pack();

    // prepare cells
    let total_sudt_amount = 424242424242424242u128;
    let input_out_point = context.create_cell(
        CellOutput::new_builder()
            .capacity((1000 * BYTE_SHANNONS).pack())
            .lock(lock_script.clone())
            .type_(Some(type_script.clone()).pack())
            .build(),
        total_sudt_amount.to_le_bytes().to_vec().into(),
    );
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .build();
    let output_lock = Script::new_builder()
        .args(Bytes::from("output_lock").pack())
        .build();
    let outputs = vec![
        CellOutput::new_builder()
            .capacity((500 * BYTE_SHANNONS).pack())
            .lock(output_lock.clone())
            .type_(Some(type_script.clone()).pack())
            .build(),
        CellOutput::new_builder()
            .capacity((500 * BYTE_SHANNONS).pack())
            .lock(output_lock.clone())
            .type_(Some(type_script.clone()).pack())
            .build(),
    ];

    let outputs_data: Vec<Bytes> = vec![(total_sudt_amount / 2).to_le_bytes().to_vec().into(); 2];

    // build transaction with revocation unlock logic
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs.clone())
        .outputs_data(outputs_data.pack())
        .build();

    // sign with revocation key
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = revocation_key
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [witness_script.clone(), vec![0xFF], signature].concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // build transaction with local_delay_epoch unlock logic
    // delay 48 hours
    let since = Since::from_epoch(EpochNumberWithFraction::new(12, 0, 1), false);
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .since(since.as_u64().pack())
        .build();

    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with local_delay_epoch_key
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = local_delay_epoch_key
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [witness_script.clone(), vec![0xFF], signature].concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // build transaction with remote_htlc_pubkey unlock offered pending htlc 1
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .build();

    let new_witness_script = [
        local_delay_epoch.as_u64().to_le_bytes().to_vec(),
        blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
        blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
        [1u8].to_vec(),
        payment_amount2.to_le_bytes().to_vec(),
        blake2b_256(preimage2)[0..20].to_vec(),
        blake2b_256(remote_htlc_key2.1.serialize())[0..20].to_vec(),
        blake2b_256(local_htlc_key2.1.serialize())[0..20].to_vec(),
        expiry2.as_u64().to_le_bytes().to_vec(),
    ]
    .concat();
    let new_lock_script = lock_script
        .clone()
        .as_builder()
        .args(blake2b_256(&new_witness_script)[0..20].to_vec().pack())
        .build();
    let outputs = vec![CellOutput::new_builder()
        .capacity((1000 * BYTE_SHANNONS).pack())
        .lock(new_lock_script.clone())
        .type_(Some(type_script.clone()).pack())
        .build()];
    let outputs_data: Vec<Bytes> = vec![(total_sudt_amount - payment_amount1)
        .to_le_bytes()
        .to_vec()
        .into()];
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with remote_htlc_pubkey
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = remote_htlc_key1
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [
        witness_script.clone(),
        vec![0x00],
        signature,
        preimage1.to_vec(),
    ]
    .concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // build transaction with local_htlc_pubkey unlock offered pending htlc 1
    let since = Since::from_timestamp(1711976400 + 1000, true).unwrap();

    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .since(since.as_u64().pack())
        .build();
    let outputs = vec![CellOutput::new_builder()
        .capacity((1000 * BYTE_SHANNONS).pack())
        .lock(new_lock_script.clone())
        .type_(Some(type_script.clone()).pack())
        .build()];
    let outputs_data: Vec<Bytes> = vec![total_sudt_amount.to_le_bytes().to_vec().into()];
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with local_htlc_pubkey
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = local_htlc_key1
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [
        witness_script.clone(),
        vec![0x00],
        signature,
        preimage1.to_vec(),
    ]
    .concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // build transaction with remote_htlc_pubkey unlock received pending htlc 2
    let since = Since::from_timestamp(1712062800 + 1000, true).unwrap();
    let input = CellInput::new_builder()
        .since(since.as_u64().pack())
        .previous_output(input_out_point.clone())
        .build();

    let new_witness_script = [
        local_delay_epoch.as_u64().to_le_bytes().to_vec(),
        blake2b_256(local_delay_epoch_key.1.serialize())[0..20].to_vec(),
        blake2b_256(revocation_key.1.serialize())[0..20].to_vec(),
        [0u8].to_vec(),
        payment_amount1.to_le_bytes().to_vec(),
        blake2b_256(preimage1)[0..20].to_vec(),
        blake2b_256(remote_htlc_key1.1.serialize())[0..20].to_vec(),
        blake2b_256(local_htlc_key1.1.serialize())[0..20].to_vec(),
        expiry1.as_u64().to_le_bytes().to_vec(),
    ]
    .concat();
    let new_lock_script = lock_script
        .as_builder()
        .args(blake2b_256(&new_witness_script)[0..20].to_vec().pack())
        .build();
    let outputs = vec![CellOutput::new_builder()
        .capacity((1000 * BYTE_SHANNONS).pack())
        .lock(new_lock_script.clone())
        .type_(Some(type_script.clone()).pack())
        .build()];
    let outputs_data: Vec<Bytes> = vec![(total_sudt_amount - payment_amount2)
        .to_le_bytes()
        .to_vec()
        .into()];
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps.clone())
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with remote_htlc_pubkey
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = remote_htlc_key2
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [witness_script.clone(), vec![0x01], signature].concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);

    // // build transaction with local_htlc_pubkey unlock received pending htlc 2
    let input = CellInput::new_builder()
        .previous_output(input_out_point.clone())
        .build();
    let outputs = vec![CellOutput::new_builder()
        .capacity((1000 * BYTE_SHANNONS).pack())
        .lock(new_lock_script.clone())
        .type_(Some(type_script.clone()).pack())
        .build()];
    let outputs_data: Vec<Bytes> = vec![total_sudt_amount.to_le_bytes().to_vec().into()];
    let tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(input)
        .outputs(outputs)
        .outputs_data(outputs_data.pack())
        .build();

    // sign with local_htlc_pubkey
    let message: [u8; 32] = tx.hash().as_slice().try_into().unwrap();

    let signature = local_htlc_key2
        .0
        .sign_recoverable(&message.into())
        .unwrap()
        .serialize();
    let witness = [
        witness_script.clone(),
        vec![0x01],
        signature,
        preimage2.to_vec(),
    ]
    .concat();

    let tx = tx.as_advanced_builder().witness(witness.pack()).build();
    println!("tx: {:?}", tx);

    // run
    let cycles = context
        .verify_tx(&tx, MAX_CYCLES)
        .expect("pass verification");
    println!("consume cycles: {}", cycles);
}
