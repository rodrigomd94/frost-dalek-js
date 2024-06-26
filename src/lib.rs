mod wrappers;

use std::alloc::{dealloc, Layout};

use curve25519_dalek::ristretto::RistrettoPoint;
use ed25519_dalek::Verifier;
use frost_dalek::{
    message_to_buffer, generate_commitment_share_lists,
    keygen::{Coefficients, RoundOne},
    precomputation::SecretCommitmentShareList,
    signature::{Initial, PartialThresholdSignature},
    DistributedKeyGeneration, IndividualPublicKey, IndividualSecretKey, Parameters, Participant,
    SignatureAggregator,
};
use napi::{bindgen_prelude::Buffer, Error, Result};
use napi_derive::napi;
use rand_core::OsRng;
use wrappers::*;

fn into_boxed_handle<T>(v: T) -> i64 {
    let bx = Box::new(v);
    Box::into_raw(bx) as i64
}

unsafe fn from_handle<T>(handle: i64) -> Box<T> {
    return Box::from_raw(handle as *mut T);
}

unsafe fn drop_handle<T>(handle: usize) {
    std::ptr::drop_in_place(handle as *mut T);
    dealloc(handle as *mut u8, Layout::new::<T>());
}

#[napi]
fn participate(uuid: u32, num_sig: u32, threshold: u32) -> ParticipateRes {
    let params = Parameters {
        n: num_sig,
        t: threshold,
    };
    let (participant, coeff) = Participant::new(&params, uuid);
    ParticipateRes {
        participant: participant.into(),
        coefficients_handle: into_boxed_handle(coeff),
    }
}

#[napi]
fn generate_their_shares_and_verify_participants(
    me: ParticipantWrapper,
    coefficients_handle: i64,
    participants: Vec<ParticipantWrapper>,
    num_sig: u32,
    threshold: u32,
) -> Result<ShareRes> {
    let params = Parameters {
        n: num_sig,
        t: threshold,
    };
    let mut participants = participants
        .into_iter()
        .map(|p| {
            let participant: Option<Participant> = p.into();
            let participant = participant?;

            let pubk = participant.public_key()?;
            participant
                .proof_of_secret_key
                .verify(&participant.index, &pubk)
                .ok()?;
            Some(participant)
        })
        .collect::<Option<Vec<Participant>>>()
        .ok_or_else(|| Error::from_reason::<String>("failed to verify participants!".into()))?;

    let coeff: Box<Coefficients> = unsafe { from_handle(coefficients_handle) };
    let me_state =
        DistributedKeyGeneration::<_>::new(&params, &me.index, &coeff, &mut participants).map_err(
            |e| {
                Error::from_reason(format!(
                    "failed to generate distributed key. misbehaving participants: {:?}",
                    e
                ))
            },
        )?;

    let their_secret_shares = me_state
        .their_secret_shares()
        .map_err(|_| Error::from_reason::<String>("failed to get secret shares".into()))?;

    Ok(ShareRes {
        their_secret_shares: their_secret_shares
            .into_iter()
            .map(|s| s.clone().into())
            .collect(),
        state_handle: into_boxed_handle(me_state),
    })
}

#[napi]
fn derive_pubk_and_group_key(
    state_handle: i64,
    me: ParticipantWrapper,
    my_secret_shares: Vec<SecretShareWrapper>,
) -> Result<DeriveRes> {
    let my_secret_shares = my_secret_shares
        .into_iter()
        .map(|s| s.into())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::from_reason::<String>("invalid secret shares".into()))?;

    let my_state: Box<DistributedKeyGeneration<RoundOne>> = unsafe { from_handle(state_handle) };
    let my_state = my_state
        .to_round_two(my_secret_shares)
        .map_err(|_| Error::from_reason::<String>("failed to move to round two".into()))?;

    let participant: Option<Participant> = me.into();
    let participant =
        participant.ok_or_else(|| Error::from_reason::<String>("invalid participant".into()))?;
    let pubk = participant
        .public_key()
        .ok_or_else(|| Error::from_reason::<String>("failed to get public key".into()))?;

    let (group_key, secret_key) = my_state
        .finish(&pubk)
        .map_err(|_| Error::from_reason::<String>("failed to finish key generation".into()))?;

    Ok(DeriveRes {
        gk: group_key.to_bytes().to_vec().into(),
        pubk: secret_key.to_public().into(),
        sk: secret_key.into(),
    })
}

#[napi]
fn gen_commitment_share_lists(uuid: u32) -> GenCommitmentShareRes {
    let (pub_comm_share, secret_comm) = generate_commitment_share_lists(&mut OsRng, uuid, 1);
    GenCommitmentShareRes {
        public_comm_share: pub_comm_share.into(),
        secret_comm_share_handle: into_boxed_handle(secret_comm),
    }
}

#[napi]
fn discard_secret_share_handle(handle: i64) {
    unsafe { drop_handle::<SecretShareWrapper>(handle as usize) };
}

#[napi]
fn get_aggregator_signers(
    threshold: u32,
    num_sig: u32,
    group_key: Buffer,
    message: Buffer,
    commitments: Vec<DualRistrettoWrap>,
    public_keys: Vec<PublicKeyWrapper>,
) -> Result<GenAggregatorRes> {
    let gk = group_key_from_buff(group_key)
        .ok_or_else(|| Error::from_reason::<String>("invalid group key".into()))?;

    let mut aggregator = SignatureAggregator::new(
        Parameters {
            n: num_sig,
            t: threshold,
        },
        gk,
        message.to_vec(),
    );

    for (commitment, pubk) in commitments.into_iter().zip(public_keys.into_iter()) {
        let commitment: Option<(RistrettoPoint, RistrettoPoint)> = commitment.into();
        let commitment = commitment
            .ok_or_else(|| Error::from_reason::<String>("invalid commitment provided".into()))?;
        let pubk: Option<IndividualPublicKey> = pubk.into();
        let pubk =
            pubk.ok_or_else(|| Error::from_reason::<String>("invalid public key provided".into()))?;
        aggregator.include_signer(pubk.index, commitment, pubk);
    }

    let signers = aggregator.get_signers().clone();
    let aggregator_handle = into_boxed_handle::<SignatureAggregator<Initial>>(aggregator);

    Ok(GenAggregatorRes {
        signers: signers.into_iter().map(|v| v.into()).collect(),
        aggregator_handle,
    })
}

#[napi]
fn sign_partial(
    secret_key: SecretKeyWrapper,
    group_key: Buffer,
    message: Buffer,
    secret_comm_share_handle: i64,
    signers: Vec<SignerWrapper>,
) -> Result<PartialThresholdSigWrapper> {
    let sk: Option<IndividualSecretKey> = secret_key.into();
    let sk = sk.ok_or_else(|| Error::from_reason::<String>("invalid secret key".into()))?;

    let gk = group_key_from_buff(group_key)
        .ok_or_else(|| Error::from_reason::<String>("invalid group key".into()))?;

    let message_hash = message_to_buffer( &message);
    let mut secret_comm_share: Box<SecretCommitmentShareList> =
        unsafe { from_handle(secret_comm_share_handle) };

    sk.sign(
        &message_hash,
        &gk,
        &mut secret_comm_share,
        0,
        &signers
            .into_iter()
            .map(|v| v.into())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Error::from_reason::<String>("invalid signers".into()))?,
    )
    .map(|sig| sig.into())
    .map_err(|e| Error::from_reason(format!("failed to sign message {}", e)))
}

#[napi]
fn aggregate_signatures(
    aggreator_handle: i64,
    signatures: Vec<PartialThresholdSigWrapper>,
) -> Result<Buffer> {
    let mut aggregator: Box<SignatureAggregator<Initial>> =
        unsafe { from_handle(aggreator_handle) };
    for signature in signatures {
        let sig: Option<PartialThresholdSignature> = signature.into();
        let sig =
            sig.ok_or_else(|| Error::from_reason::<String>("invalid partial signatures".into()))?;
        aggregator.include_partial_signature(sig);
    }
    let aggregator = aggregator
        .finalize()
        .map_err(|_| Error::from_reason::<String>("failed to finalize aggregation".into()))?;
    let sig = aggregator
        .aggregate()
        .map_err(|_| Error::from_reason::<String>("failed to aggregate signatures".into()))?;

    return Ok(sig.to_ed25519().to_vec().into());
}

#[napi]
fn validate_signature(
    group_key: Buffer,
    signature: Buffer,
    message: Buffer,
) -> Result<()> {
    let gk = group_key_from_buff(group_key)
        .ok_or_else(|| Error::from_reason::<String>("invalid group key".into()))?;

    let gk_ed = ed25519_dalek::PublicKey::from_bytes(&gk.to_ed25519()).unwrap();

    let message_hash = message_to_buffer( &message);
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&signature);
    let sig_ed = ed25519_dalek::Signature::from(sig);

    gk_ed.verify(&message_hash, &sig_ed).map_err(|_| {
        Error::from_reason::<String>("threshold signature verification failed!".into())
    })?;

    Ok(())
}

#[napi]
fn group_key_to_ed25519(group_key: Buffer) -> Result<Buffer> {
    let gk = group_key_from_buff(group_key)
        .ok_or_else(|| Error::from_reason::<String>("invalid group key".into()))?;

    return Ok(gk.to_ed25519().to_vec().into());
}
