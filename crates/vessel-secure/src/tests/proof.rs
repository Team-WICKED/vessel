//! The relay's possession proof: only the key's holder answers, an answer
//! fits only its own challenge, and proofs and channels cannot be mixed.

use futures_util::{SinkExt as _, StreamExt as _};
use vessel_proto::HandshakeMessage;

use super::support::carriers;
use crate::{NodeKeypair, SecureError, answer_proof, initiate, proof_challenge, respond};

#[test]
fn the_holder_of_the_key_proves_it() {
    let keypair = NodeKeypair::generate().unwrap();
    let (challenge, message) = proof_challenge(keypair.public()).unwrap();
    let answer = answer_proof(&keypair, &message).unwrap();
    assert!(challenge.verify(&answer));
}

#[test]
fn an_impostor_cannot_answer_for_someone_elses_key() {
    let victim = NodeKeypair::generate().unwrap();
    let impostor = NodeKeypair::generate().unwrap();
    let (challenge, message) = proof_challenge(victim.public()).unwrap();
    // The impostor cannot even read the challenge...
    assert!(matches!(
        answer_proof(&impostor, &message),
        Err(SecureError::Handshake(_))
    ));
    // ...and what it can make, an answer to a challenge for its own key,
    // does not prove the victim's.
    let (_own, own_message) = proof_challenge(impostor.public()).unwrap();
    let answer = answer_proof(&impostor, &own_message).unwrap();
    assert!(!challenge.verify(&answer));
}

#[test]
fn an_answer_fits_only_its_own_challenge() {
    let keypair = NodeKeypair::generate().unwrap();
    let (first, first_message) = proof_challenge(keypair.public()).unwrap();
    let (second, _second_message) = proof_challenge(keypair.public()).unwrap();
    let answer = answer_proof(&keypair, &first_message).unwrap();
    assert!(!second.verify(&answer));
    assert!(first.verify(&answer));
}

#[test]
fn malformed_answers_are_refused() {
    let keypair = NodeKeypair::generate().unwrap();
    for answer in [vec![], vec![0; 47], vec![0; 48], vec![0; 49]] {
        let (challenge, _message) = proof_challenge(keypair.public()).unwrap();
        assert!(!challenge.verify(&HandshakeMessage::new(answer).unwrap()));
    }
    let challenge = HandshakeMessage::new(vec![1; 48]).unwrap();
    assert!(answer_proof(&keypair, &challenge).is_err());
}

#[tokio::test]
async fn a_proof_challenge_is_no_channel_handshake() {
    // A relay cannot pass its challenge to a node's `/secure` endpoint as a
    // client: the prologues differ.
    let keypair = NodeKeypair::generate().unwrap();
    let (_challenge, message) = proof_challenge(keypair.public()).unwrap();
    let (mut relay, node_end) = carriers();
    relay
        .send(bytes::Bytes::copy_from_slice(message.as_bytes()))
        .await
        .unwrap();
    let node = respond(node_end, &keypair).await;
    assert!(matches!(node, Err(SecureError::Handshake(_))), "{node:?}");
}

#[tokio::test]
async fn a_channel_handshake_is_no_proof_challenge() {
    // Nor can anyone get a proof answer out of a node by starting a channel.
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, mut node_end) = carriers();
    let take_first = async {
        let first = node_end.next().await.unwrap().unwrap();
        drop(node_end);
        first
    };
    let (_client, first) = tokio::join!(initiate(client_end, keypair.public()), take_first);
    let first = HandshakeMessage::new(first.to_vec()).unwrap();
    assert!(matches!(
        answer_proof(&keypair, &first),
        Err(SecureError::Handshake(_))
    ));
}

#[test]
fn a_challenge_can_wait_across_tasks() {
    fn send<T: Send + 'static>() {}
    send::<crate::ProofChallenge>();
    send::<crate::SecureChannel<crate::MemoryCarrier>>();
}
