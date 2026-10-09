// SPDX-License-Identifier: GPL-3.0-or-later
//! Each seat's attestation of the run's result (design §3.5, plan §10.8-9, §10.13).

use ed25519_dalek::SigningKey;
use molt_core::put_bytes;
use molt_treasury::attest::{self, Attested};
use molt_treasury::{Network, RunId, TreasuryError};

const ADDRESS: &str = "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A";

fn seats(n: u8) -> Vec<SigningKey> {
    (1..=n).map(|k| SigningKey::from_bytes(&[k; 32])).collect()
}

fn pks(sks: &[SigningKey]) -> Vec<String> {
    sks.iter()
        .map(|s| hex::encode(s.verifying_key().to_bytes()))
        .collect()
}

fn fields() -> Attested<'static> {
    Attested {
        run: RunId {
            republic_id: [1; 32],
            init_id: 5,
            run: [2; 32],
        },
        transcript: [3; 32],
        address: ADDRESS,
        network: Network::Mainnet,
        m: 2,
        n: 3,
        birthday: 3_200_000,
    }
}

#[test]
fn attestations_verify_only_for_their_seat_and_fields() {
    let sks = seats(3);
    let a = fields();
    let sigs: Vec<_> = sks.iter().map(|s| attest::sign(s, &a)).collect();
    attest::verify_all(&a, &pks(&sks), &sigs).expect("all n verify");

    let mut swapped = sigs.clone();
    swapped.swap(0, 1);
    assert_eq!(
        attest::verify_all(&a, &pks(&sks), &swapped),
        Err(TreasuryError::Attestation(1))
    );
    assert_eq!(
        attest::verify_all(&a, &pks(&sks), &sigs[..2]),
        Err(TreasuryError::Participants)
    );
    assert_eq!(
        attest::verify_all(&a, &pks(&sks[..2]), &sigs[..2]),
        Err(TreasuryError::Participants)
    );
    let mut bad_pk = pks(&sks);
    bad_pk[2] = "zz".into();
    assert_eq!(
        attest::verify_all(&a, &bad_pk, &sigs),
        Err(TreasuryError::Attestation(3))
    );

    let variants = [
        Attested {
            transcript: [4; 32],
            ..fields()
        },
        Attested {
            address: "4",
            ..fields()
        },
        Attested {
            network: Network::Stagenet,
            ..fields()
        },
        Attested { m: 1, ..fields() },
        Attested {
            birthday: 3_200_001,
            ..fields()
        },
    ];
    for v in variants {
        assert_eq!(
            attest::verify_all(&v, &pks(&sks), &sigs),
            Err(TreasuryError::Attestation(1))
        );
    }
}

#[test]
fn an_attestation_from_another_run_or_init_fails() {
    let sks = seats(3);
    let a = fields();
    let sigs: Vec<_> = sks.iter().map(|s| attest::sign(s, &a)).collect();
    for run in [
        RunId {
            run: [9; 32],
            ..a.run
        },
        RunId {
            init_id: 6,
            ..a.run
        },
        RunId {
            republic_id: [9; 32],
            ..a.run
        },
    ] {
        let other = Attested { run, ..fields() };
        assert_eq!(
            attest::verify_all(&other, &pks(&sks), &sigs),
            Err(TreasuryError::Attestation(1))
        );
    }
}

#[test]
fn the_attestation_layout_is_pinned() {
    let a = fields();
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-attest-v1");
    want.extend_from_slice(&[1; 32]);
    want.extend_from_slice(&5u64.to_le_bytes());
    want.extend_from_slice(&[2; 32]);
    want.extend_from_slice(&[3; 32]);
    put_bytes(&mut want, ADDRESS.as_bytes());
    want.push(0);
    want.extend_from_slice(&[2, 0, 3, 0]);
    want.extend_from_slice(&3_200_000u64.to_le_bytes());
    assert_eq!(attest::attestation_bytes(&a), want);
    for (net, b) in [
        (Network::Mainnet, 0u8),
        (Network::Testnet, 1),
        (Network::Stagenet, 2),
    ] {
        let bytes = attest::attestation_bytes(&Attested {
            network: net,
            ..fields()
        });
        assert_eq!(bytes[4 + 21 + 32 + 8 + 32 + 32 + 4 + ADDRESS.len()], b);
    }
}

/// OpenMLS signs `SignContent { label<V> = "MLS 1.0 " + Label, content<V> }`
/// with the same identity key; `<V>` is the RFC 9000 varint length. Read as
/// one, our bytes must never yield that label prefix.
#[test]
fn attest_tag_cannot_collide_with_openmls_sign_content() {
    let bytes = attest::attestation_bytes(&fields());
    let first = bytes[0];
    let (len, at): (usize, usize) = match first >> 6 {
        0 => (usize::from(first & 0x3f), 1),
        1 => (usize::from(first & 0x3f) << 8 | usize::from(bytes[1]), 2),
        2 => (
            usize::try_from(u32::from_be_bytes([
                first & 0x3f,
                bytes[1],
                bytes[2],
                bytes[3],
            ]))
            .expect("len"),
            4,
        ),
        _ => (usize::MAX, 8),
    };
    let label = bytes.get(at..at.saturating_add(len)).unwrap_or(&[]);
    assert!(
        !label.starts_with(b"MLS 1.0 "),
        "attestation bytes parse as an OpenMLS label"
    );
    assert!(!bytes[..16].windows(8).any(|w| w == b"MLS 1.0 "));
}
